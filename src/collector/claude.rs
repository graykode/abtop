use super::process::{self, ProcInfo};
use crate::model::{AgentSession, ChildProcess, SessionFile, SessionStatus, SubAgent};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(all(not(target_os = "linux"), not(target_vendor = "apple")))]
use std::process::Command;

/// A single Claude config directory (sessions + projects + transcripts).
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct ConfigDir {
    sessions_dir: PathBuf,
    projects_dir: PathBuf,
}

impl ConfigDir {
    fn new(base: PathBuf) -> Self {
        Self {
            sessions_dir: base.join("sessions"),
            projects_dir: base.join("projects"),
        }
    }

    fn base_dir(&self) -> PathBuf {
        self.sessions_dir
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf()
    }
}

#[derive(Debug, Default)]
struct ProcessOpenPaths {
    cwd: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

pub struct ClaudeCollector {
    /// All known config directories to scan for sessions.
    config_dirs: Vec<ConfigDir>,
    /// Cached transcript parse results keyed by session_id.
    /// On each tick, only new bytes since `new_offset` are parsed.
    transcript_cache: HashMap<String, TranscriptResult>,
}

impl ClaudeCollector {
    pub fn new() -> Self {
        Self {
            config_dirs: Vec::new(),
            transcript_cache: HashMap::new(),
        }
    }

    /// Discover all unique Claude config directories by reading
    /// /proc/<pid>/environ for each running Claude process.
    /// Always includes the default (~/.claude) and CLAUDE_CONFIG_DIR if set.
    fn refresh_config_dirs(&mut self, process_info: &HashMap<u32, process::ProcInfo>) {
        // BTreeSet for deterministic iteration order across runs.
        let mut seen = std::collections::BTreeSet::new();

        // Always include the default directory
        let default = dirs::home_dir().unwrap_or_default().join(".claude");
        seen.insert(default);

        // Include CLAUDE_CONFIG_DIR from abtop's own environment
        if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
            let p = PathBuf::from(dir);
            if p.is_dir() {
                seen.insert(p);
            }
        }

        // Discover from running Claude processes via /proc/<pid>/environ
        for (pid, info) in process_info {
            if !process::cmd_has_binary(&info.command, "claude") {
                continue;
            }
            if let Some(dir) = read_env_var_from_proc(*pid, "CLAUDE_CONFIG_DIR") {
                let p = PathBuf::from(dir);
                if p.is_dir() {
                    seen.insert(p);
                }
            }
        }

        self.config_dirs = seen.into_iter().map(ConfigDir::new).collect();
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        // Refresh config dirs on slow ticks only (every ~10s) or on first run.
        // Scanning /proc/<pid>/environ for every Claude process is expensive
        // to do every 2s; config dirs change rarely.
        if shared.slow_tick || self.config_dirs.is_empty() {
            self.refresh_config_dirs(&shared.process_info);
        }

        let active_session_paths = self.discover_active_session_paths(&shared.process_info);
        let active_config_dirs: Vec<ConfigDir> = active_session_paths
            .iter()
            .map(|(_, config)| config.clone())
            .collect();
        self.merge_config_dirs(active_config_dirs);

        // Collect all session file paths first to avoid borrowing self
        // immutably (config_dirs) and mutably (load_session) at the same time.
        let mut session_paths: Vec<(PathBuf, ConfigDir)> = Vec::new();
        session_paths.extend(active_session_paths);

        for config in &self.config_dirs {
            let session_files = match fs::read_dir(&config.sessions_dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            for entry in session_files.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                session_paths.push((path, config.clone()));
            }
        }

        let discovery_ctx = build_discovery_context(&session_paths, &shared.process_info);

        let mut sessions = self.load_session_paths(
            &session_paths,
            &shared.process_info,
            &shared.children_map,
            &shared.ports,
            &discovery_ctx,
        );

        self.evict_stale_cache(&sessions);

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Drop `transcript_cache` entries for session_ids that are no longer
    /// in the active set. After `/clear`, the old sid leaves the active
    /// set and its cache entry (with stale token counters) is removed on
    /// the very next tick — without this, counters would persist forever.
    fn evict_stale_cache(&mut self, sessions: &[AgentSession]) {
        let active_ids: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.session_id.as_str()).collect();
        self.transcript_cache
            .retain(|sid, _| active_ids.contains(sid.as_str()));
    }

    fn load_session_paths(
        &mut self,
        session_paths: &[(PathBuf, ConfigDir)],
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
        ctx: &DiscoveryContext,
    ) -> Vec<AgentSession> {
        let mut sessions = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for (path, config) in session_paths {
            if let Some(session) =
                self.load_session(path, config, process_info, children_map, ports, ctx)
            {
                if seen_ids.insert(session.session_id.clone()) {
                    sessions.push(session);
                }
            }
        }
        sessions
    }

    fn merge_config_dirs(&mut self, dirs: Vec<ConfigDir>) {
        let mut seen: std::collections::BTreeSet<ConfigDir> =
            self.config_dirs.iter().cloned().collect();
        seen.extend(dirs);
        self.config_dirs = seen.into_iter().collect();
    }

    fn discover_active_session_paths(
        &self,
        process_info: &HashMap<u32, process::ProcInfo>,
    ) -> Vec<(PathBuf, ConfigDir)> {
        let pids = Self::find_claude_pids(process_info);
        if pids.is_empty() {
            return Vec::new();
        }

        let open_paths = Self::map_pid_to_open_paths(&pids);
        Self::session_paths_from_open_paths(&pids, &open_paths)
    }

    fn session_paths_from_open_paths(
        pids: &[u32],
        open_paths: &HashMap<u32, ProcessOpenPaths>,
    ) -> Vec<(PathBuf, ConfigDir)> {
        let mut paths = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        for &pid in pids {
            let Some(info) = open_paths.get(&pid) else {
                continue;
            };
            for config in config_dirs_from_open_paths(info) {
                let Some(path) = find_session_file_for_pid(&config.sessions_dir, pid) else {
                    continue;
                };
                if seen.insert(path.clone()) {
                    paths.push((path, config));
                }
            }
        }

        paths
    }

    fn find_claude_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
        let mut pids = Vec::new();
        for (pid, info) in process_info {
            let cmd = &info.command;
            if process::cmd_has_binary(cmd, "claude") && !cmd.contains("--print") {
                pids.push(*pid);
            }
        }
        pids
    }

    fn map_pid_to_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
        if pids.is_empty() {
            return HashMap::new();
        }

        #[cfg(target_os = "linux")]
        {
            map_pid_to_proc_open_paths(pids)
        }

        #[cfg(target_vendor = "apple")]
        {
            map_pid_to_libproc_open_paths(pids)
        }

        #[cfg(all(not(target_os = "linux"), not(target_vendor = "apple")))]
        {
            map_pid_to_lsof_open_paths(pids)
        }
    }

    fn load_session(
        &mut self,
        path: &Path,
        config: &ConfigDir,
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
        ctx: &DiscoveryContext,
    ) -> Option<AgentSession> {
        let content = fs::read_to_string(path).ok()?;
        let mut sf: SessionFile = serde_json::from_str(&content).ok()?;
        sf.sanitize();

        // Resolve the project dir that actually holds this session's
        // transcripts. For worktree sessions the on-disk dir does not match
        // `encode_cwd_path(cwd)`, so locate it via the original sid first.
        let project_dir = resolve_project_dir(config, &sf.cwd, &sf.session_id);

        // `/clear` mints a new sessionId + transcript without rewriting
        // `sessions/{PID}.json`. Override with the most recent transcript in
        // the project dir so counters/status track the live session instead
        // of the stale one. (#68)
        //
        // Skip the override when multiple active claude PIDs share this
        // cwd: we can't tell which PID owns a freshly-created jsonl, and
        // picking the wrong one would cross-contaminate counters. Also
        // exclude sids already claimed by OTHER session files (a sibling
        // PID's transcript must not be adopted as this PID's live sid).
        let siblings = ctx.pids_per_cwd.get(&sf.cwd).copied().unwrap_or(1);
        if siblings <= 1 {
            let excluded: std::collections::HashSet<&str> = ctx
                .claimed_sids_by_pid
                .iter()
                .filter(|&(p, _)| *p != sf.pid)
                .map(|(_, s)| s.as_str())
                .collect();
            if let Some(live_sid) =
                find_live_session_id(project_dir.as_deref(), sf.started_at, &excluded)
            {
                if live_sid != sf.session_id {
                    sf.session_id = live_sid;
                }
            }
        }

        let proc_cmd = process_info.get(&sf.pid).map(|p| p.command.as_str());
        let pid_alive = proc_cmd
            .map(|c| process::cmd_has_binary(c, "claude"))
            .unwrap_or(false);

        // Skip --print sessions (e.g. abtop's own summary generation).
        // Only filter while process is alive (command visible); dead sessions
        // are cleaned up when the session file disappears.
        if proc_cmd.map(|c| c.contains("--print")).unwrap_or(false) {
            return None;
        }

        let project_name = sf.cwd.rsplit('/').next().unwrap_or("?").to_string();

        let proc = process_info.get(&sf.pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        // Use the already-resolved project_dir so a post-/clear sid lookup
        // lands in the same (possibly worktree) directory as the original.
        let transcript_path = project_dir.as_ref().and_then(|pd| {
            let p = pd.join(format!("{}.jsonl", sf.session_id));
            if p.exists() && !is_symlink(&p) {
                Some(p)
            } else {
                None
            }
        });

        if let Some(ref tp) = transcript_path {
            let cached = self.transcript_cache.remove(&sf.session_id);
            // Detect file replacement: if inode or mtime changed, reparse from scratch
            let identity_changed = cached
                .as_ref()
                .map(|c| c.file_identity != file_identity(tp))
                .unwrap_or(false);
            let from_offset = if identity_changed {
                0
            } else {
                cached.as_ref().map(|c| c.new_offset).unwrap_or(0)
            };

            let delta = parse_transcript(tp, from_offset);

            if let Some(mut prev) = cached {
                // File replaced, shrank, or first parse — replace entirely
                if identity_changed || from_offset == 0 || delta.new_offset < from_offset {
                    self.transcript_cache.insert(sf.session_id.clone(), delta);
                } else {
                    // Merge delta into cached result
                    if delta.model != "-" {
                        prev.model = delta.model;
                    }
                    prev.total_input += delta.total_input;
                    prev.total_output += delta.total_output;
                    prev.total_cache_read += delta.total_cache_read;
                    prev.total_cache_create += delta.total_cache_create;
                    if delta.last_context_tokens > 0 {
                        prev.last_context_tokens = delta.last_context_tokens;
                    }
                    if delta.max_context_tokens > prev.max_context_tokens {
                        prev.max_context_tokens = delta.max_context_tokens;
                    }
                    prev.turn_count += delta.turn_count;
                    // Always update current_task from delta — empty means
                    // latest assistant turn had no tool_use (task cleared)
                    if delta.turn_count > 0 {
                        prev.current_task = delta.current_task;
                    }
                    if !delta.version.is_empty() {
                        prev.version = delta.version;
                    }
                    if !delta.git_branch.is_empty() {
                        prev.git_branch = delta.git_branch;
                    }
                    if delta.last_activity > prev.last_activity {
                        prev.last_activity = delta.last_activity;
                    }
                    prev.token_history.extend(delta.token_history);
                    if prev.tool_calls.len() < 500 {
                        let remaining = 500 - prev.tool_calls.len();
                        prev.tool_calls.extend(delta.tool_calls.into_iter().take(remaining));
                    }
                    // Only overwrite turn-state when the delta actually
                    // observed new user/assistant lines. A no-op tick (file
                    // didn't grow) returns an empty delta whose zeroed
                    // timestamps would otherwise wipe the live markers and
                    // break the timeline animation between polls.
                    if delta.saw_turn {
                        prev.last_assistant_ts_ms = delta.last_assistant_ts_ms;
                        prev.last_user_ts_ms = delta.last_user_ts_ms;
                    }
                    if prev.initial_prompt.is_empty() && !delta.initial_prompt.is_empty() {
                        prev.initial_prompt = delta.initial_prompt;
                    }
                    prev.new_offset = delta.new_offset;
                    self.transcript_cache.insert(sf.session_id.clone(), prev);
                }
            } else {
                // First parse — store full result
                self.transcript_cache.insert(sf.session_id.clone(), delta);
            }
        }

        let empty_result = TranscriptResult {
            model: "-".to_string(),
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            last_context_tokens: 0,
            max_context_tokens: 0,
            context_history: Vec::new(),
            compaction_count: 0,
            turn_count: 0,
            current_task: String::new(),
            version: String::new(),
            git_branch: String::new(),
            last_activity: std::time::UNIX_EPOCH,
            new_offset: 0,
            file_identity: (0, 0),
            token_history: Vec::new(),
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            tool_calls: Vec::new(), last_assistant_ts_ms: 0, last_user_ts_ms: 0,
            saw_turn: false,
        };
        let cached = self
            .transcript_cache
            .get(&sf.session_id)
            .unwrap_or(&empty_result);

        let model = cached.model.clone();
        let total_input = cached.total_input;
        let total_output = cached.total_output;
        let total_cache_read = cached.total_cache_read;
        let total_cache_create = cached.total_cache_create;
        let last_context_tokens = cached.last_context_tokens;
        let max_context_tokens = cached.max_context_tokens;
        let turn_count = cached.turn_count;
        let current_task = cached.current_task.clone();
        let version = cached.version.clone();
        let git_branch = cached.git_branch.clone();
        let last_activity = cached.last_activity;
        let token_history = cached.token_history.clone();
        let context_history = cached.context_history.clone();
        let compaction_count = cached.compaction_count;
        let initial_prompt = cached.initial_prompt.clone();
        let first_assistant_text = cached.first_assistant_text.clone();
        let tool_calls = cached.tool_calls.clone();

        if !pid_alive {
            return None;
        }

        let status = {
            let since_activity = std::time::SystemTime::now()
                .duration_since(last_activity)
                .unwrap_or_default();
            if since_activity.as_secs() < 30 {
                SessionStatus::Working
            } else {
                // Transcript is stale (>30s). Check CPU-based signals:
                // 1. Claude process using CPU > 1% → likely thinking/streaming
                let claude_cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
                // 2. Any descendant using significant CPU (>5%) → likely running a tool
                //    (higher threshold avoids false positives from idle watchers/servers)
                let has_active_descendant =
                    process::has_active_descendant(sf.pid, children_map, process_info, 5.0);
                if claude_cpu_active || has_active_descendant {
                    SessionStatus::Working
                } else {
                    SessionStatus::Waiting
                }
            }
        };

        let context_window = context_window_for_model(&model, max_context_tokens);
        let context_percent = if context_window > 0 {
            (last_context_tokens as f64 / context_window as f64) * 100.0
        } else {
            0.0
        };

        let current_tasks = if !current_task.is_empty() {
            vec![current_task]
        } else if !pid_alive {
            vec!["finished".to_string()]
        } else if matches!(status, SessionStatus::Waiting) {
            vec!["waiting for input".to_string()]
        } else {
            vec!["thinking...".to_string()]
        };

        let mut children = Vec::new();
        // Collect all descendants (not just direct children) so we catch
        // grandchild processes that listen on ports (e.g. Claude → shell → node).
        let mut stack: Vec<u32> = children_map.get(&sf.pid).cloned().unwrap_or_default();
        let mut visited = std::collections::HashSet::new();
        while let Some(cpid) = stack.pop() {
            if !visited.insert(cpid) {
                continue;
            }
            if let Some(cproc) = process_info.get(&cpid) {
                let port = ports.get(&cpid).and_then(|v| v.first().copied());
                children.push(ChildProcess {
                    pid: cpid,
                    command: cproc.command.clone(),
                    mem_kb: cproc.rss_kb,
                    port,
                });
            }
            // Add grandchildren to the stack
            if let Some(grandchildren) = children_map.get(&cpid) {
                stack.extend(grandchildren);
            }
        }

        // Git stats: populated by MultiCollector on slow ticks
        let (git_added, git_modified) = (0, 0);

        // Derive the project directory from the transcript path (handles worktree sessions),
        // falling back to the encoded cwd.
        let project_dir = transcript_path
            .as_ref()
            .and_then(|tp| tp.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| config.projects_dir.join(encode_cwd_path(&sf.cwd)));

        // Subagent discovery
        let subagents_dir = project_dir.join(&sf.session_id).join("subagents");
        let subagents = Self::collect_subagents(&subagents_dir);

        // Memory status
        let memory_dir = project_dir.join("memory");
        let (mem_file_count, mem_line_count) = Self::collect_memory_status(&memory_dir);

        // Effort level (persistent `effortLevel` from settings files).
        // Note: the `/effort` slash command is session-scoped and does NOT persist,
        // so this only reflects settings.json — not live in-session overrides.
        let effort = read_effort_level(&sf.cwd);

        Some(AgentSession {
            agent_cli: "claude",
            pid: sf.pid,
            session_id: sf.session_id,
            cwd: sf.cwd,
            project_name,
            started_at: sf.started_at,
            status,
            model,
            effort,
            context_percent,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cache_read,
            total_cache_create,
            turn_count,
            current_tasks,
            mem_mb,
            version,
            git_branch,
            git_added,
            git_modified,
            token_history,
            context_history,
            compaction_count,
            context_window,
            subagents,
            mem_file_count,
            mem_line_count,
            children,
            initial_prompt,
            first_assistant_text,
            tool_calls,
            pending_since_ms: cached.last_assistant_ts_ms,
            thinking_since_ms: cached.last_user_ts_ms,
        })
    }

    fn collect_subagents(subagents_dir: &Path) -> Vec<SubAgent> {
        let mut subagents = Vec::new();

        let entries = match fs::read_dir(subagents_dir) {
            Ok(e) => e,
            Err(_) => return subagents,
        };

        // Collect meta files and their corresponding jsonl files
        let mut meta_files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
                continue;
            }
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".meta.json") {
                    meta_files.push(path);
                }
            }
        }

        for meta_path in meta_files {
            let meta_name = match meta_path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            // Parse meta JSON
            let meta_content = match fs::read_to_string(&meta_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let meta_val: Value = match serde_json::from_str(&meta_content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let description = meta_val
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("agent")
                .to_string();

            // Derive jsonl path: agent-{hash}.meta.json -> agent-{hash}.jsonl
            let jsonl_name = meta_name.replace(".meta.json", ".jsonl");
            let jsonl_path = meta_path.with_file_name(&jsonl_name);

            let mut tokens = 0u64;
            let mut last_activity = std::time::UNIX_EPOCH;

            if jsonl_path.exists() {
                // Get file mtime for status
                if let Ok(metadata) = fs::metadata(&jsonl_path) {
                    if let Ok(mtime) = metadata.modified() {
                        last_activity = mtime;
                    }
                }

                // Parse jsonl for token totals
                let transcript = parse_transcript(&jsonl_path, 0);
                tokens = transcript.total_input
                    + transcript.total_output
                    + transcript.total_cache_read
                    + transcript.total_cache_create;
            }

            let status = {
                let since = std::time::SystemTime::now()
                    .duration_since(last_activity)
                    .unwrap_or_default();
                if since.as_secs() < 30 {
                    "working".to_string()
                } else {
                    "done".to_string()
                }
            };

            // Use description as name, shorten if needed
            let name = truncate(&description, 30);

            subagents.push(SubAgent {
                name,
                status,
                tokens,
            });
        }

        subagents
    }

    fn collect_memory_status(memory_dir: &Path) -> (u32, u32) {
        let mut file_count = 0u32;
        let mut line_count = 0u32;

        if let Ok(entries) = fs::read_dir(memory_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    file_count += 1;
                }
            }
        }

        let memory_md = memory_dir.join("MEMORY.md");
        if let Ok(content) = fs::read_to_string(&memory_md) {
            line_count = content.lines().count() as u32;
        }

        (file_count, line_count)
    }
}

impl super::AgentCollector for ClaudeCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }

    fn discovered_config_dirs(&self) -> Vec<PathBuf> {
        self.config_dirs.iter().map(ConfigDir::base_dir).collect()
    }
}

#[cfg(target_os = "linux")]
fn map_pid_to_proc_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    let mut map = HashMap::new();

    for &pid in pids {
        let cwd = fs::read_link(format!("/proc/{}/cwd", pid)).ok();
        let entries = match fs::read_dir(format!("/proc/{}/fd", pid)) {
            Ok(entries) => entries,
            Err(_) => {
                if cwd.is_some() {
                    map.insert(
                        pid,
                        ProcessOpenPaths {
                            cwd,
                            paths: Vec::new(),
                        },
                    );
                }
                continue;
            }
        };

        let paths = entries
            .flatten()
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|path| path.is_absolute())
            .collect();
        map.insert(pid, ProcessOpenPaths { cwd, paths });
    }

    map
}

#[cfg(target_vendor = "apple")]
fn map_pid_to_libproc_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    use proc_pidinfo::{
        proc_pidfdinfo, proc_pidinfo_list, Pid, ProcFDInfo, ProcFDType, VnodeFdInfoWithPath,
    };

    let mut map = HashMap::new();

    for &raw_pid in pids {
        let pid = Pid(raw_pid);
        let fds = match proc_pidinfo_list::<ProcFDInfo>(pid) {
            Ok(fds) => fds,
            Err(_) => continue,
        };

        let paths = fds
            .into_iter()
            .filter(|fd| fd.fd_type() == Ok(ProcFDType::VNODE))
            .filter_map(|fd| proc_pidfdinfo::<VnodeFdInfoWithPath>(pid, fd.proc_fd).ok())
            .flatten()
            .filter_map(|vnode| vnode.path().ok().map(PathBuf::from))
            .collect();

        map.insert(raw_pid, ProcessOpenPaths { cwd: None, paths });
    }

    map
}

#[cfg(all(not(target_os = "linux"), not(target_vendor = "apple")))]
fn map_pid_to_lsof_open_paths(pids: &[u32]) -> HashMap<u32, ProcessOpenPaths> {
    let pid_args: Vec<String> = pids.iter().map(|p| format!("-p{}", p)).collect();
    let mut args = vec!["-F", "ftn"];
    for pa in &pid_args {
        args.push(pa);
    }

    let output = Command::new("lsof").args(&args).output().ok();
    output
        .map(|out| parse_lsof_process_info(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
}

#[cfg_attr(any(target_os = "linux", target_vendor = "apple"), allow(dead_code))]
fn parse_lsof_process_info(output: &str) -> HashMap<u32, ProcessOpenPaths> {
    let mut map: HashMap<u32, ProcessOpenPaths> = HashMap::new();
    let mut current_pid: Option<u32> = None;
    let mut current_fd = String::new();

    for line in output.lines() {
        if let Some(pid_str) = line.strip_prefix('p') {
            current_pid = pid_str.parse::<u32>().ok();
            if let Some(pid) = current_pid {
                map.entry(pid).or_default();
            }
            current_fd.clear();
        } else if let Some(fd) = line.strip_prefix('f') {
            current_fd = fd.to_string();
        } else if let Some(name) = line.strip_prefix('n') {
            let Some(pid) = current_pid else {
                continue;
            };
            if name.is_empty() || name.starts_with('[') {
                continue;
            }
            let path = PathBuf::from(name);
            let info = map.entry(pid).or_default();
            if current_fd == "cwd" {
                info.cwd = Some(path.clone());
            }
            info.paths.push(path);
        }
    }

    map
}

fn config_dirs_from_open_paths(info: &ProcessOpenPaths) -> Vec<ConfigDir> {
    let mut candidates = Vec::new();
    if let Some(cwd) = &info.cwd {
        candidates.push(cwd.clone());
    }
    candidates.extend(info.paths.iter().cloned());

    let mut roots = std::collections::BTreeSet::new();
    for path in candidates {
        for root in candidate_config_roots_from_path(&path) {
            if is_claude_config_root(&root) {
                roots.insert(root);
            }
        }
    }

    roots.into_iter().map(ConfigDir::new).collect()
}

fn candidate_config_roots_from_path(path: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.push(path.to_path_buf());

    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if cursor.file_name().and_then(|n| n.to_str()) == Some("sessions")
            || cursor.file_name().and_then(|n| n.to_str()) == Some("projects")
        {
            roots.push(parent.to_path_buf());
        }
        cursor = parent;
    }

    roots
}

fn is_claude_config_root(path: &Path) -> bool {
    path.join("sessions").is_dir() && path.join("projects").is_dir()
}

/// Per-tick session-discovery state shared across all `load_session` calls
/// in a single `collect_sessions` pass. Pre-parsed so each PID can reason
/// about the sids claimed by its neighbors without re-reading every
/// session file.
#[derive(Default)]
struct DiscoveryContext {
    /// PID → sid currently recorded in its `sessions/{PID}.json`.
    claimed_sids_by_pid: HashMap<u32, String>,
    /// cwd → number of active session files pointing at it.
    pids_per_cwd: HashMap<String, usize>,
}

fn build_discovery_context(
    session_paths: &[(PathBuf, ConfigDir)],
    process_info: &HashMap<u32, ProcInfo>,
) -> DiscoveryContext {
    let mut claimed_sids_by_pid: HashMap<u32, String> = HashMap::new();
    let mut pids_per_cwd: HashMap<String, usize> = HashMap::new();
    let mut seen_pids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (path, _) in session_paths {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(mut sf) = serde_json::from_str::<SessionFile>(&content) else {
            continue;
        };
        sf.sanitize();
        if !seen_pids.insert(sf.pid) {
            continue;
        }
        // Only count PIDs that are alive AND actually claude AND not a
        // `--print` spawn. Stale `sessions/{PID}.json` files (crashed
        // sessions) and abtop's own `claude --print` summary children
        // would otherwise inflate `pids_per_cwd` and silently suppress
        // the /clear sid override for the real session sharing that cwd.
        let Some(info) = process_info.get(&sf.pid) else {
            continue;
        };
        if !process::cmd_has_binary(&info.command, "claude") {
            continue;
        }
        if info.command.contains("--print") {
            continue;
        }
        *pids_per_cwd.entry(sf.cwd.clone()).or_insert(0) += 1;
        claimed_sids_by_pid.insert(sf.pid, sf.session_id);
    }
    DiscoveryContext {
        claimed_sids_by_pid,
        pids_per_cwd,
    }
}

/// Resolve the project directory that holds this session's transcripts.
/// Prefers `encode_cwd_path(cwd)` but falls back to any sibling subdir
/// containing `{original_sid}.jsonl` — worktree sessions live under a dir
/// keyed by the branch name, not the encoded cwd, so both the transcript
/// path and the live-sid lookup must resolve through this fallback.
fn resolve_project_dir(config: &ConfigDir, cwd: &str, original_sid: &str) -> Option<PathBuf> {
    let encoded = encode_cwd_path(cwd);
    let primary = config.projects_dir.join(&encoded);
    let jsonl_name = format!("{}.jsonl", original_sid);

    let primary_has_original = {
        let p = primary.join(&jsonl_name);
        p.exists() && !is_symlink(&p)
    };
    if primary_has_original {
        return Some(primary);
    }

    if let Ok(entries) = fs::read_dir(&config.projects_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let candidate = path.join(&jsonl_name);
            if candidate.exists() && !is_symlink(&candidate) {
                return Some(path);
            }
        }
    }

    // Original transcript is missing (deleted, or never flushed yet). Fall
    // back to the encoded-cwd dir if it exists so live-sid lookup still
    // has somewhere to scan.
    if primary.is_dir() {
        return Some(primary);
    }
    None
}

/// Find the currently-live session_id by scanning the project directory
/// for the most recently modified transcript.
///
/// `/clear` mints a new sessionId and a new `{sid}.jsonl` without rewriting
/// `sessions/{PID}.json`, so the session file's sid goes stale. The fresh
/// transcript is however always present on disk in the same project dir.
/// `started_at_ms` filters out transcripts older than this PID's lifetime
/// (prior runs or sibling claudes that left files behind). `excluded`
/// skips sids already claimed by other active session files.
fn find_live_session_id(
    project_dir: Option<&Path>,
    started_at_ms: u64,
    excluded: &std::collections::HashSet<&str>,
) -> Option<String> {
    let project_dir = project_dir?;
    let entries = fs::read_dir(project_dir).ok()?;

    // Allow a small grace window (5s) before started_at to tolerate clock
    // skew between the session file's startedAt and jsonl creation mtime
    // (FS mtime granularity is 1-2s on some platforms, and startedAt is
    // captured before Claude Code flushes the transcript's first line).
    let min_mtime = std::time::UNIX_EPOCH
        + std::time::Duration::from_millis(started_at_ms.saturating_sub(5_000));

    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in entries.flatten() {
        if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if excluded.contains(stem) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < min_mtime {
            continue;
        }
        match &best {
            Some((best_mtime, _)) if mtime <= *best_mtime => {}
            _ => best = Some((mtime, stem.to_string())),
        }
    }

    best.map(|(_, sid)| sid)
}

fn find_session_file_for_pid(sessions_dir: &Path, pid: u32) -> Option<PathBuf> {
    let direct = sessions_dir.join(format!("{}.json", pid));
    if direct.exists() && !is_symlink(&direct) {
        return Some(direct);
    }

    let entries = fs::read_dir(sessions_dir).ok()?;
    for entry in entries.flatten() {
        if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Skip files we can't read or parse — one bad file shouldn't abort
        // the whole fallback search (previously `?` bubbled out of the loop).
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<SessionFile>(&content) else {
            continue;
        };
        if session.pid == pid {
            return Some(path);
        }
    }

    None
}

struct TranscriptResult {
    model: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    /// Last assistant turn's input context size (for context % calculation)
    last_context_tokens: u64,
    /// High-water mark: largest context seen in any turn (for 1M detection)
    max_context_tokens: u64,
    /// Per-turn context sizes for evolution visualization.
    context_history: Vec<u64>,
    /// Detected compaction events (context dropped > 30% between consecutive turns).
    compaction_count: u32,
    turn_count: u32,
    current_task: String,
    version: String,
    git_branch: String,
    last_activity: std::time::SystemTime,
    new_offset: u64,
    /// File identity: (inode, mtime_ns). Used to detect file replacement
    /// even when the new file is the same size or larger.
    file_identity: (u64, u64),
    token_history: Vec<u64>,
    initial_prompt: String,
    /// First assistant response text (text blocks only, no tool_use)
    first_assistant_text: String,
    /// Tool call timeline extracted from transcript.
    tool_calls: Vec<crate::model::ToolCall>,
    /// Timestamp of the last assistant turn (epoch ms), used to compute tool duration.
    last_assistant_ts_ms: u64,
    /// Timestamp (epoch ms) of the most recent `user` line that has not been
    /// followed by an assistant turn. Zero when the latest entry was an
    /// assistant turn. Mutually exclusive with `last_assistant_ts_ms`.
    last_user_ts_ms: u64,
    /// True when this parse observed at least one `user` or `assistant`
    /// line. When false the timestamp fields above are just defaults and
    /// must not overwrite cached state — otherwise a no-new-data tick
    /// would clear the live pending/thinking markers.
    saw_turn: bool,
}

/// Check if a path is a symlink without following it.
/// Defaults to true on error (fail-closed: skip if we can't verify).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

/// Get file identity as (inode, mtime_nanos) for detecting file replacement.
fn file_identity(path: &Path) -> (u64, u64) {
    fs::metadata(path)
        .ok()
        .map(|m| {
            let ino = m.ino();
            let mtime_ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (ino, mtime_ns)
        })
        .unwrap_or((0, 0))
}

fn parse_transcript(path: &Path, from_offset: u64) -> TranscriptResult {
    let identity = file_identity(path);
    let mut result = TranscriptResult {
        model: "-".to_string(),
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        total_cache_create: 0,
        last_context_tokens: 0,
        max_context_tokens: 0,
        context_history: Vec::new(),
        compaction_count: 0,
        turn_count: 0,
        current_task: String::new(),
        version: String::new(),
        git_branch: String::new(),
        last_activity: std::time::UNIX_EPOCH,
        new_offset: from_offset,
        file_identity: identity,
        token_history: Vec::new(),
        initial_prompt: String::new(),
        first_assistant_text: String::new(),
        tool_calls: Vec::new(),
        last_assistant_ts_ms: 0,
        last_user_ts_ms: 0,
        saw_turn: false,
    };

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return result,
    };

    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file_len == from_offset {
        // No new data
        result.new_offset = file_len;
        return result;
    }
    // File shrank (truncation/rotation) — reset and reparse from start
    let effective_offset = if file_len < from_offset {
        0
    } else {
        from_offset
    };
    let from_offset = effective_offset;

    let mut reader = BufReader::new(file);
    if from_offset > 0 {
        let _ = reader.seek(SeekFrom::Start(from_offset));
    }

    let mtime = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(std::time::UNIX_EPOCH);
    result.last_activity = mtime;

    const MAX_LINE_BYTES: usize = 10 * 1024 * 1024; // 10 MB

    let mut bytes_read = from_offset;
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        // Bounded read: take(MAX+1) physically caps the allocation. Without
        // this, a malformed or hostile transcript with an unbounded line
        // would OOM the process before any length check could fire.
        match reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_line(&mut line_buf)
        {
            Ok(0) => break,
            Ok(n) => {
                // Cap hit without a newline — malformed/hostile line. Skip
                // to end of file; we'll re-evaluate when file_identity changes.
                if line_buf.len() > MAX_LINE_BYTES && !line_buf.ends_with('\n') {
                    bytes_read = file_len;
                    break;
                }
                let has_newline = line_buf.ends_with('\n');
                let line = line_buf.trim();
                if line.is_empty() {
                    if has_newline {
                        bytes_read += n as u64;
                    }
                    continue;
                }
                // Try to parse as JSON. If incomplete (no newline) and
                // parse fails, defer to next poll. If parse succeeds,
                // accept the record even without trailing newline.
                let val = match serde_json::from_str::<Value>(line) {
                    Ok(v) => v,
                    Err(_) => {
                        if has_newline {
                            // Complete line but invalid JSON — skip it
                            bytes_read += n as u64;
                        }
                        // Incomplete line with parse error — defer
                        if !has_newline {
                            break;
                        }
                        continue;
                    }
                };
                bytes_read += n as u64;
                {
                    // Parse timestamp from any entry (for tool duration calculation)
                    let entry_ts_ms = val.get("timestamp")
                        .and_then(|t| t.as_str())
                        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                        .map(|dt| dt.timestamp_millis().max(0) as u64)
                        .unwrap_or(0);

                    match val.get("type").and_then(|t| t.as_str()) {
                        Some("assistant") => {
                            result.turn_count += 1;
                            // Clear previous task on each new turn so stale tasks
                            // don't persist when latest turn has no tool_use
                            result.current_task = String::new();
                            if let Some(msg) = val.get("message") {
                                if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                                    result.model = m.to_string();
                                }
                                if let Some(usage) = msg.get("usage") {
                                    let inp = usage
                                        .get("input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let out = usage
                                        .get("output_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let cr = usage
                                        .get("cache_read_input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let cc = usage
                                        .get("cache_creation_input_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    result.total_input += inp;
                                    result.total_output += out;
                                    result.total_cache_read += cr;
                                    result.total_cache_create += cc;
                                    // Context = input_tokens + cache_read (excludes cache_creation, #54)
                                    let prev_context = result.last_context_tokens;
                                    result.last_context_tokens = inp + cr;
                                    if result.last_context_tokens > result.max_context_tokens {
                                        result.max_context_tokens = result.last_context_tokens;
                                    }
                                    // Detect compaction: context drops > 30% between turns
                                    if prev_context > 0
                                        && result.last_context_tokens < prev_context * 7 / 10
                                    {
                                        result.compaction_count += 1;
                                    }
                                    if result.context_history.len() < 10_000 {
                                        result.context_history.push(result.last_context_tokens);
                                    }
                                    if result.token_history.len() < 10_000 {
                                        result.token_history.push(inp + out + cr + cc);
                                    }
                                }
                                // Extract first assistant text (text blocks only) for summary fallback
                                if result.first_assistant_text.is_empty() {
                                    if let Some(content) =
                                        msg.get("content").and_then(|c| c.as_array())
                                    {
                                        let texts: Vec<&str> = content
                                            .iter()
                                            .filter_map(|block| {
                                                if block.get("type").and_then(|t| t.as_str())
                                                    == Some("text")
                                                {
                                                    block.get("text").and_then(|t| t.as_str())
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect();
                                        if !texts.is_empty() {
                                            let joined = texts.join(" ");
                                            let normalized: String = joined
                                                .lines()
                                                .map(|l| l.trim())
                                                .filter(|l| !l.is_empty())
                                                .collect::<Vec<_>>()
                                                .join(" ");
                                            result.first_assistant_text =
                                                truncate(&normalized, 200);
                                        }
                                    }
                                }
                                // Extract ALL tool_use entries for timeline + last for current_task
                                if let Some(content) = msg.get("content").and_then(|c| c.as_array())
                                {
                                    for item in content {
                                        if item.get("type").and_then(|t| t.as_str())
                                            == Some("tool_use")
                                        {
                                            let tool = item
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("?");
                                            let arg = extract_tool_arg(item);
                                            result.current_task = format!("{} {}", tool, arg);
                                            if result.tool_calls.len() < 500 {
                                                result.tool_calls.push(crate::model::ToolCall {
                                                    name: tool.to_string(),
                                                    arg: truncate(&arg, 40),
                                                    duration_ms: 0, // filled on next user turn
                                                });
                                            }
                                        }
                                    }
                                }
                                // Save timestamp for duration calculation
                                if entry_ts_ms > 0 {
                                    result.last_assistant_ts_ms = entry_ts_ms;
                                }
                                // Any assistant turn closes the prior "thinking" window.
                                result.last_user_ts_ms = 0;
                                result.saw_turn = true;
                            }
                        }
                        Some("user") => {
                            // Compute tool call duration: time from assistant turn to this user turn
                            if entry_ts_ms > 0 && result.last_assistant_ts_ms > 0 {
                                let duration = entry_ts_ms.saturating_sub(result.last_assistant_ts_ms);
                                // Distribute duration across tool calls from that assistant turn
                                // (approximation: divide equally among pending zero-duration calls)
                                let pending: Vec<usize> = result.tool_calls.iter().enumerate()
                                    .rev()
                                    .take_while(|(_, tc)| tc.duration_ms == 0)
                                    .map(|(i, _)| i)
                                    .collect();
                                if !pending.is_empty() {
                                    let per_call = duration / pending.len() as u64;
                                    for idx in pending {
                                        result.tool_calls[idx].duration_ms = per_call;
                                    }
                                }
                                result.last_assistant_ts_ms = 0;
                            }
                            // Mark the start of a thinking window — next assistant
                            // turn clears it. Covers both prompts and tool_result
                            // lines (both serialize as `user` type).
                            if entry_ts_ms > 0 {
                                result.last_user_ts_ms = entry_ts_ms;
                            }
                            result.saw_turn = true;
                            if let Some(v) = val.get("version").and_then(|v| v.as_str()) {
                                result.version = v.to_string();
                            }
                            if let Some(b) = val.get("gitBranch").and_then(|b| b.as_str()) {
                                result.git_branch = b.to_string();
                            }
                            // Extract first user prompt as session title
                            if result.initial_prompt.is_empty() {
                                if let Some(msg) = val.get("message") {
                                    result.initial_prompt = extract_prompt_text(msg);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(_) => break,
        }
    }

    result.new_offset = bytes_read;
    result
}

/// Extract a short summary from the first user message content.
/// Handles both string content and array-of-blocks content.
/// Encode a cwd path to match Claude Code's project directory naming.
/// Claude Code replaces '/', '_', and '.' with '-'.
fn encode_cwd_path(cwd: &str) -> String {
    cwd.chars()
        .map(|c| match c {
            '/' | '_' | '.' => '-',
            _ => c,
        })
        .collect()
}

fn extract_prompt_text(message: &Value) -> String {
    let raw = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            // Find first text block
            arr.iter()
                .filter_map(|block| {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        block
                            .get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .next()
                .unwrap_or_default()
        }
        _ => return String::new(),
    };

    // Clean up: remove image markers, code blocks, markdown headers
    let cleaned: String = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ");

    // Remove [Image #N] markers
    let mut result = cleaned;
    while let Some(start) = result.find("[Image") {
        if let Some(end) = result[start..].find(']') {
            result = format!(
                "{}{}",
                &result[..start],
                result[start + end + 1..].trim_start()
            );
        } else {
            break;
        }
    }

    let clean = result.trim().to_string();
    if clean.is_empty() {
        return String::new();
    }
    // Skip prompts generated by abtop's own summary generation (claude --print)
    if clean.contains("You are a conversation title generator") {
        return String::new();
    }
    truncate(&clean, 50)
}

fn extract_tool_arg(tool_use: &Value) -> String {
    if let Some(input) = tool_use.get("input") {
        // Edit/Read: file_path
        if let Some(fp) = input.get("file_path").and_then(|f| f.as_str()) {
            return shorten_path(fp);
        }
        // Bash: command (first 40 chars, redact secrets)
        if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
            let short = cmd.lines().next().unwrap_or(cmd);
            return super::redact_secrets(&truncate(short, 40));
        }
        // Grep/Glob: pattern
        if let Some(pat) = input.get("pattern").and_then(|p| p.as_str()) {
            return truncate(pat, 40);
        }
    }
    String::new()
}

fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        format!("{}/{}", parts[1], parts[0])
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{}…", truncated)
    }
}

fn context_window_for_model(model: &str, last_context_tokens: u64) -> u64 {
    if model.contains("[1m]") || last_context_tokens > 200_000 {
        1_000_000
    } else {
        200_000
    }
}

/// Read the persistent `effortLevel` for a Claude Code session.
///
/// Precedence (highest wins), matching Claude Code's own resolution order:
/// 1. `CLAUDE_CODE_EFFORT_LEVEL` env var (abtop's own env — only visible when
///    set in the user's shell before launching both abtop and claude)
/// 2. `{cwd}/.claude/settings.local.json`
/// 3. `{cwd}/.claude/settings.json`
/// 4. `~/.claude/settings.local.json`
/// 5. `~/.claude/settings.json`
///
/// Returns an empty string when no `effortLevel` is set. This does NOT capture
/// in-session `/effort` changes — those are ephemeral and not written to disk.
fn read_effort_level(cwd: &str) -> String {
    if let Ok(v) = std::env::var("CLAUDE_CODE_EFFORT_LEVEL") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    let cwd_path = PathBuf::from(cwd);
    candidates.push(cwd_path.join(".claude").join("settings.local.json"));
    candidates.push(cwd_path.join(".claude").join("settings.json"));
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".claude").join("settings.local.json"));
        candidates.push(home.join(".claude").join("settings.json"));
    }

    for path in candidates {
        if let Some(level) = read_effort_from_settings(&path) {
            return level;
        }
    }
    String::new()
}

fn read_effort_from_settings(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let level = val.get("effortLevel")?.as_str()?.trim();
    if level.is_empty() {
        None
    } else {
        Some(level.to_string())
    }
}

/// Parse a NUL-separated environ blob to extract a single variable's value.
/// Only invoked from the Linux-gated `read_env_var_from_proc`; kept available
/// to unit tests on all platforms, so suppress dead_code on non-Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_environ_var(data: &[u8], var_name: &str) -> Option<String> {
    let prefix = format!("{}=", var_name);
    data.split(|&b| b == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find(|entry| entry.starts_with(&prefix))
        .map(|entry| entry[prefix.len()..].to_string())
}

/// Read a single environment variable from a running process via /proc/<pid>/environ.
/// Returns None if the process is inaccessible or the variable is not set.
#[cfg(target_os = "linux")]
fn read_env_var_from_proc(pid: u32, var_name: &str) -> Option<String> {
    let data = fs::read(format!("/proc/{}/environ", pid)).ok()?;
    parse_environ_var(&data, var_name)
}

/// Stub for non-Linux platforms where /proc is not available.
#[cfg(not(target_os = "linux"))]
fn read_env_var_from_proc(_pid: u32, _var_name: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_lines(file: &mut tempfile::NamedTempFile, lines: &[&str]) {
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
    }

    fn write_session_file(path: &Path, pid: u32, session_id: &str, cwd: &Path) {
        std::fs::write(
            path,
            format!(
                r#"{{"pid":{},"sessionId":"{}","cwd":"{}","startedAt":1774715116826}}"#,
                pid,
                session_id,
                cwd.display()
            ),
        )
        .unwrap();
    }

    fn write_transcript(projects: &Path, cwd: &Path, session_id: &str, prompt: &str) -> PathBuf {
        let transcript_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join(format!("{}.jsonl", session_id));
        std::fs::write(
            &transcript,
            format!(
                r#"{{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{{"role":"user","content":"{}"}}}}
{{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{{"model":"claude-sonnet-4-6","usage":{{"input_tokens":12,"output_tokens":6,"cache_read_input_tokens":3,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"done"}}]}}}}
"#,
                prompt
            ),
        )
        .unwrap();
        transcript
    }

    fn make_proc_info(pid: u32, command: &str) -> HashMap<u32, ProcInfo> {
        let mut process_info = HashMap::new();
        process_info.insert(
            pid,
            ProcInfo {
                pid,
                ppid: 1,
                rss_kb: 2048,
                cpu_pct: 0.0,
                command: command.to_string(),
            },
        );
        process_info
    }

    #[test]
    fn test_parse_transcript_no_new_bytes_does_not_set_saw_turn() {
        // Regression for the merge fix: a no-op poll (from_offset == file_len)
        // must return saw_turn=false so the cached pending/thinking markers
        // aren't clobbered by the default-zero timestamps.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","id":"t1","input":{"file_path":"x"}}]}}"#,
        ]);
        let file_len = std::fs::metadata(file.path()).unwrap().len();

        let result = parse_transcript(file.path(), file_len);

        assert!(!result.saw_turn);
        assert_eq!(result.last_user_ts_ms, 0);
        assert_eq!(result.last_assistant_ts_ms, 0);
        assert_eq!(result.new_offset, file_len);
    }

    #[test]
    fn test_parse_transcript_non_turn_lines_do_not_set_saw_turn() {
        // A delta that only processes non-user/non-assistant entries (e.g.
        // `summary` lines emitted on compaction) must also leave saw_turn
        // false, so the merge step preserves the cached turn state.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"summary","summary":"compaction marker","leafUuid":"abc"}"#,
        ]);

        let result = parse_transcript(file.path(), 0);

        assert!(!result.saw_turn);
        assert_eq!(result.last_user_ts_ms, 0);
        assert_eq!(result.last_assistant_ts_ms, 0);
        assert!(result.new_offset > 0, "non-turn lines still advance offset");
    }

    #[test]
    fn test_parse_transcript_user_then_assistant_clears_thinking_marker() {
        // Sanity check on the mutual exclusion: after an assistant turn
        // closes a thinking window, last_user_ts_ms must be zero and
        // last_assistant_ts_ms must carry the assistant timestamp.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","id":"t1","input":{"file_path":"x"}}]}}"#,
        ]);

        let result = parse_transcript(file.path(), 0);

        assert!(result.saw_turn);
        assert_eq!(result.last_user_ts_ms, 0);
        assert!(result.last_assistant_ts_ms > 0);
    }

    #[test]
    fn test_parse_transcript_trailing_user_marks_thinking_window() {
        // When the latest line is a user turn (prompt or tool_result),
        // last_user_ts_ms should carry its timestamp so the UI can render
        // the live Think row.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:00Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}"#,
            r#"{"type":"user","timestamp":"2026-03-28T15:00:10Z","message":{"role":"user","content":"next"}}"#,
        ]);

        let result = parse_transcript(file.path(), 0);

        assert!(result.saw_turn);
        assert!(result.last_user_ts_ms > 0);
        assert_eq!(result.last_assistant_ts_ms, 0);
    }

    #[test]
    fn test_parse_lsof_process_info_captures_multiple_pids_and_cwd() {
        let output = "\
p111
fcwd
tDIR
n/Users/alice/project
f15
tDIR
n/Users/alice/.claude-work
p222
fcwd
tDIR
n/Users/bob/project
f20
tREG
n/Users/bob/.claude-alt/projects/-Users-bob-project/session.jsonl
";

        let parsed = parse_lsof_process_info(output);

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.get(&111).unwrap().cwd.as_deref(),
            Some(Path::new("/Users/alice/project")),
        );
        assert!(parsed.get(&222).unwrap().paths.contains(&PathBuf::from(
            "/Users/bob/.claude-alt/projects/-Users-bob-project/session.jsonl"
        )));
    }

    #[test]
    fn test_config_dirs_from_open_paths_finds_profile_root_and_ignores_project_claude_dir() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::create_dir_all(profile.join("projects")).unwrap();

        let project_claude = temp.path().join("repo").join(".claude");
        std::fs::create_dir_all(&project_claude).unwrap();
        std::fs::write(project_claude.join("settings.local.json"), "{}").unwrap();

        let info = ProcessOpenPaths {
            cwd: Some(temp.path().join("repo")),
            paths: vec![
                project_claude,
                profile.clone(),
                profile
                    .join("projects")
                    .join("-tmp-repo")
                    .join("session.jsonl"),
            ],
        };

        let configs = config_dirs_from_open_paths(&info);

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].base_dir(), profile);
    }

    #[test]
    fn test_config_dirs_from_open_paths_finds_profile_root_without_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        let projects = profile.join("projects");
        std::fs::create_dir_all(&projects).unwrap();

        let info = ProcessOpenPaths {
            cwd: None,
            paths: vec![projects.join("-tmp-repo").join("session.jsonl")],
        };

        let configs = config_dirs_from_open_paths(&info);

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].base_dir(), profile);
    }

    #[test]
    fn test_session_paths_from_open_paths_maps_pid_to_session_file() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 4242;
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, "session-4242", &cwd);

        let mut open_paths = HashMap::new();
        open_paths.insert(
            pid,
            ProcessOpenPaths {
                cwd: None,
                paths: vec![projects.join("-tmp-repo").join("session-4242.jsonl")],
            },
        );

        let discovered = ClaudeCollector::session_paths_from_open_paths(&[pid], &open_paths);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].0, session_path);
        assert_eq!(discovered[0].1.base_dir(), profile);
    }

    #[test]
    fn test_session_paths_from_open_paths_deduplicates_same_session_path() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 4242;
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, "session-4242", &cwd);

        let mut open_paths = HashMap::new();
        open_paths.insert(
            pid,
            ProcessOpenPaths {
                cwd: None,
                paths: vec![
                    projects.join("-tmp-repo").join("session-4242.jsonl"),
                    sessions.join(format!("{}.json", pid)),
                    profile.clone(),
                ],
            },
        );

        let discovered = ClaudeCollector::session_paths_from_open_paths(&[pid], &open_paths);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].0, session_path);
    }

    #[test]
    fn test_collect_sessions_deduplicates_active_and_scanned_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 4242;
        let session_id = "session-4242";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, session_id, &cwd);
        write_transcript(&projects, &cwd, session_id, "dedup prompt");

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];
        let session_paths = vec![
            (session_path.clone(), config.clone()),
            (session_path.clone(), config),
        ];
        let ctx = build_discovery_context(&session_paths, &process_info);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, session_id);
    }

    #[test]
    fn test_find_session_file_for_pid_falls_back_to_embedded_pid() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let session_path = sessions.join("custom-name.json");
        write_session_file(&session_path, 4242, "session-4242", &cwd);

        assert_eq!(
            find_session_file_for_pid(&sessions, 4242).as_deref(),
            Some(session_path.as_path()),
        );
    }

    #[test]
    fn test_find_session_file_for_pid_skips_bad_files_and_continues() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        // A malformed JSON file that appears before the real one on-disk.
        // Previously the `?` operator made one bad file abort the whole
        // fallback search; now the loop must skip it and still find the match.
        std::fs::write(sessions.join("aaa-broken.json"), "not json at all").unwrap();
        let target = sessions.join("zzz-valid.json");
        write_session_file(&target, 9999, "session-9999", &cwd);

        assert_eq!(
            find_session_file_for_pid(&sessions, 9999).as_deref(),
            Some(target.as_path()),
        );
    }

    #[test]
    fn test_find_session_file_for_pid_prefers_direct_pid_file() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let direct = sessions.join("4242.json");
        let fallback = sessions.join("custom-name.json");
        write_session_file(&direct, 4242, "direct", &cwd);
        write_session_file(&fallback, 4242, "fallback", &cwd);

        assert_eq!(
            find_session_file_for_pid(&sessions, 4242).as_deref(),
            Some(direct.as_path()),
        );
    }

    #[test]
    fn test_find_session_file_for_pid_rejects_symlinked_session_files() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let real_direct = temp.path().join("real-direct.json");
        write_session_file(&real_direct, 4242, "direct", &cwd);
        std::os::unix::fs::symlink(&real_direct, sessions.join("4242.json")).unwrap();

        let real_fallback = temp.path().join("real-fallback.json");
        write_session_file(&real_fallback, 4242, "fallback", &cwd);
        std::os::unix::fs::symlink(&real_fallback, sessions.join("fallback.json")).unwrap();

        assert!(find_session_file_for_pid(&sessions, 4242).is_none());
    }

    #[test]
    fn test_find_claude_pids_excludes_print_sessions() {
        let mut process_info = HashMap::new();
        process_info.insert(
            10,
            ProcInfo {
                pid: 10,
                ppid: 1,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "claude".to_string(),
            },
        );
        process_info.insert(
            11,
            ProcInfo {
                pid: 11,
                ppid: 1,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "claude --print summarize".to_string(),
            },
        );
        process_info.insert(
            12,
            ProcInfo {
                pid: 12,
                ppid: 1,
                rss_kb: 1,
                cpu_pct: 0.0,
                command: "codex".to_string(),
            },
        );

        assert_eq!(ClaudeCollector::find_claude_pids(&process_info), vec![10]);
    }

    #[test]
    fn test_resolve_project_dir_uses_worktree_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let projects = profile.join("projects");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::create_dir_all(&projects).unwrap();

        // The transcript lives in a dir that does NOT match encode_cwd_path.
        let worktree_dir = projects.join("actual-worktree");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::write(worktree_dir.join("session-1.jsonl"), "{}\n").unwrap();

        let config = ConfigDir::new(profile);

        assert_eq!(
            resolve_project_dir(&config, "/tmp/repo", "session-1").as_deref(),
            Some(worktree_dir.as_path()),
        );
    }

    #[test]
    fn test_resolve_project_dir_rejects_symlinked_matches() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let session_id = "session-1";
        let real_exact = temp.path().join("real-exact.jsonl");
        std::fs::write(&real_exact, "{}\n").unwrap();
        let exact_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&exact_dir).unwrap();
        std::os::unix::fs::symlink(&real_exact, exact_dir.join(format!("{}.jsonl", session_id)))
            .unwrap();

        let real_fallback = temp.path().join("real-fallback.jsonl");
        std::fs::write(&real_fallback, "{}\n").unwrap();
        let fallback_dir = projects.join("actual-worktree");
        std::fs::create_dir_all(&fallback_dir).unwrap();
        std::os::unix::fs::symlink(
            &real_fallback,
            fallback_dir.join(format!("{}.jsonl", session_id)),
        )
        .unwrap();

        let config = ConfigDir::new(profile);

        // Both exact and fallback are symlinks and must be rejected. The
        // encoded-cwd dir exists though, so the final fallback returns it.
        assert_eq!(
            resolve_project_dir(&config, cwd.to_str().unwrap(), session_id).as_deref(),
            Some(exact_dir.as_path()),
        );
    }

    #[test]
    fn test_open_paths_without_cwd_loads_session_from_same_config_root() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 5151;
        let session_id = "session-5151";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, session_id, &cwd);
        let transcript = write_transcript(&projects, &cwd, session_id, "libproc prompt");

        let mut open_paths = HashMap::new();
        open_paths.insert(
            pid,
            ProcessOpenPaths {
                cwd: None,
                paths: vec![transcript],
            },
        );
        let discovered = ClaudeCollector::session_paths_from_open_paths(&[pid], &open_paths);
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();

        assert_eq!(discovered.len(), 1);
        let ctx = build_discovery_context(&discovered, &process_info);
        let session = collector
            .load_session(
                &discovered[0].0,
                &discovered[0].1,
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &ctx,
            )
            .unwrap();

        assert_eq!(session.session_id, session_id);
        assert_eq!(session.initial_prompt, "libproc prompt");
        assert_eq!(session.total_input_tokens, 12);
    }

    #[test]
    fn test_load_session_uses_non_default_config_root() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude-work");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 5151;
        let session_id = "session-5151";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, session_id, &cwd);

        let transcript_dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(
            transcript_dir.join(format!("{}.jsonl", session_id)),
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","version":"2.1.90","gitBranch":"main","message":{"role":"user","content":"profile specific prompt"}}
{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":12,"output_tokens":6,"cache_read_input_tokens":3,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"done"}]}}
"#,
        )
        .unwrap();

        let mut collector = ClaudeCollector::new();
        let process_info = make_proc_info(pid, "claude");
        let children_map = HashMap::new();
        let ports = HashMap::new();
        let config = ConfigDir::new(profile);
        let ctx = build_discovery_context(&[(session_path.clone(), config.clone())], &process_info);

        let session = collector
            .load_session(
                &session_path,
                &config,
                &process_info,
                &children_map,
                &ports,
                &ctx,
            )
            .unwrap();

        assert_eq!(session.session_id, session_id);
        assert_eq!(session.cwd, cwd.to_str().unwrap());
        assert_eq!(session.total_input_tokens, 12);
        assert_eq!(session.total_cache_read, 3);
        assert_eq!(session.initial_prompt, "profile specific prompt");
    }

    #[test]
    fn test_parse_transcript_basic_tokens() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","version":"2.1.86","gitBranch":"main","message":{"role":"user","content":"fix the bug"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":30},"content":[{"type":"text","text":"I found the issue."}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.total_input, 100);
        assert_eq!(result.total_output, 50);
        assert_eq!(result.total_cache_read, 200);
        assert_eq!(result.total_cache_create, 30);
        assert_eq!(result.model, "claude-sonnet-4-6");
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.last_context_tokens, 300); // 100 + 200 (cache_creation excluded)
    }

    #[test]
    fn test_parse_transcript_multiple_turns() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first prompt"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"First response."}]}}"#,
                r#"{"type":"user","timestamp":"2026-03-28T15:01:00Z","message":{"role":"user","content":"second prompt"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":200,"output_tokens":80,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Second response."}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.turn_count, 2);
        assert_eq!(result.total_input, 300); // 100 + 200
        assert_eq!(result.total_output, 130); // 50 + 80
        assert_eq!(result.token_history.len(), 2);
    }

    #[test]
    fn test_parse_transcript_tool_use_current_task() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"fix the bug"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs"}}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.current_task, "Edit src/main.rs");
    }

    #[test]
    fn test_parse_transcript_initial_prompt() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"refactor the auth module"}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.initial_prompt, "refactor the auth module");
    }

    #[test]
    fn test_parse_transcript_incremental_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first prompt"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"First response."}]}}"#,
            ],
        );
        let first = parse_transcript(file.path(), 0);
        let offset = first.new_offset;
        assert!(offset > 0);

        // Append a third line (new assistant turn)
        write_lines(
            &mut file,
            &[
                r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":40,"output_tokens":20,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Third."}]}}"#,
            ],
        );
        let delta = parse_transcript(file.path(), offset);
        assert_eq!(delta.turn_count, 1);
        assert_eq!(delta.total_input, 40);
        assert_eq!(delta.total_output, 20);
    }

    #[test]
    fn test_parse_transcript_empty_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.model, "-");
        assert_eq!(result.total_input, 0);
        assert_eq!(result.turn_count, 0);
    }

    #[test]
    fn test_encode_cwd_path() {
        assert_eq!(encode_cwd_path("/Users/foo/bar"), "-Users-foo-bar");
        assert_eq!(
            encode_cwd_path("/home/user/my_project.v2"),
            "-home-user-my-project-v2"
        );
    }

    #[test]
    fn test_context_window_for_model() {
        // Base model with low token usage → 200K
        assert_eq!(context_window_for_model("claude-opus-4-6", 50_000), 200_000);
        // Explicit [1m] suffix → 1M regardless of token count
        assert_eq!(
            context_window_for_model("claude-opus-4-6[1m]", 0),
            1_000_000
        );
        assert_eq!(
            context_window_for_model("claude-sonnet-4-6", 100_000),
            200_000
        );
        assert_eq!(context_window_for_model("unknown-model", 0), 200_000);
        // Token usage exceeds 200K → must be 1M window
        assert_eq!(
            context_window_for_model("claude-opus-4-6", 250_000),
            1_000_000
        );
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn test_shorten_path() {
        assert_eq!(
            shorten_path("src/collector/claude.rs"),
            "collector/claude.rs"
        );
        assert_eq!(shorten_path("main.rs"), "main.rs");
    }

    #[test]
    fn test_parse_transcript_skips_malformed_json() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
                r#"THIS IS NOT VALID JSON"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"response"}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        // Bad line should be skipped, assistant line still parsed
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.total_input, 100);
        assert_eq!(result.initial_prompt, "hi");
    }

    #[test]
    fn test_parse_transcript_file_shrunk_resets() {
        use std::io::Seek;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first"}}"#,
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"resp"}]}}"#,
            ],
        );
        let first = parse_transcript(file.path(), 0);
        let old_offset = first.new_offset;
        assert!(old_offset > 0);

        // Simulate file rotation: truncate and write shorter content
        file.as_file().set_len(0).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"assistant","timestamp":"2026-03-28T16:00:00Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"new session"}]}}"#,
            ],
        );
        // Pass old offset that is now beyond file length
        let result = parse_transcript(file.path(), old_offset);
        // Should reset to 0 and parse the new content
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.total_input, 10);
    }

    #[test]
    fn test_parse_transcript_current_task_cleared_between_turns() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                // Turn 1: has tool_use
                r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs"}}]}}"#,
                // Turn 2: text only, no tool_use
                r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Done, all changes applied."}]}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.turn_count, 2);
        // current_task should be empty because last turn had no tool_use
        assert_eq!(result.current_task, "");
    }

    #[test]
    fn test_parse_transcript_version_and_git_branch() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","version":"2.1.90","gitBranch":"feat/payments","message":{"role":"user","content":"add stripe"}}"#,
            ],
        );
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.version, "2.1.90");
        assert_eq!(result.git_branch, "feat/payments");
    }

    #[test]
    fn test_read_effort_from_settings_extracts_value() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, r#"{{"effortLevel":"high","other":true}}"#).unwrap();
        file.flush().unwrap();
        assert_eq!(
            read_effort_from_settings(file.path()).as_deref(),
            Some("high")
        );
    }

    #[test]
    fn test_read_effort_from_settings_missing_field_returns_none() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, r#"{{"permissions":{{"deny":[]}}}}"#).unwrap();
        file.flush().unwrap();
        assert!(read_effort_from_settings(file.path()).is_none());
    }

    #[test]
    fn test_read_effort_from_settings_empty_string_returns_none() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, r#"{{"effortLevel":""}}"#).unwrap();
        file.flush().unwrap();
        assert!(read_effort_from_settings(file.path()).is_none());
    }

    #[test]
    fn test_read_effort_from_settings_nonexistent_file() {
        assert!(read_effort_from_settings(Path::new("/nonexistent/nowhere.json")).is_none());
    }

    #[test]
    fn test_parse_environ_var_found() {
        let data = b"HOME=/root\0CLAUDE_CONFIG_DIR=/home/user/.claude-pro\0SHELL=/bin/bash\0";
        assert_eq!(
            parse_environ_var(data, "CLAUDE_CONFIG_DIR").as_deref(),
            Some("/home/user/.claude-pro"),
        );
    }

    #[test]
    fn test_parse_environ_var_not_set() {
        let data = b"HOME=/root\0SHELL=/bin/bash\0";
        assert!(parse_environ_var(data, "CLAUDE_CONFIG_DIR").is_none());
    }

    #[test]
    fn test_parse_environ_var_empty_value() {
        let data = b"CLAUDE_CONFIG_DIR=\0OTHER=val\0";
        assert_eq!(
            parse_environ_var(data, "CLAUDE_CONFIG_DIR").as_deref(),
            Some(""),
        );
    }

    #[test]
    fn test_parse_environ_var_no_partial_match() {
        // CLAUDE_CONFIG_DIR_EXTRA should not match CLAUDE_CONFIG_DIR
        let data = b"CLAUDE_CONFIG_DIR_EXTRA=/wrong\0";
        assert!(parse_environ_var(data, "CLAUDE_CONFIG_DIR").is_none());
    }

    fn set_mtime(path: &Path, secs_from_now: i64) {
        let t = if secs_from_now >= 0 {
            std::time::SystemTime::now() + std::time::Duration::from_secs(secs_from_now as u64)
        } else {
            std::time::SystemTime::now()
                - std::time::Duration::from_secs((-secs_from_now) as u64)
        };
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    #[test]
    fn test_find_live_session_id_picks_newest_jsonl() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let old_path = project_dir.join("old-sid.jsonl");
        let new_path = project_dir.join("new-sid.jsonl");
        std::fs::write(&old_path, "{}\n").unwrap();
        std::fs::write(&new_path, "{}\n").unwrap();
        set_mtime(&old_path, -60);
        set_mtime(&new_path, 0);

        let started_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(120_000);

        let sid = find_live_session_id(
            Some(project_dir.as_path()),
            started_ms,
            &std::collections::HashSet::new(),
        );
        assert_eq!(sid.as_deref(), Some("new-sid"));
    }

    #[test]
    fn test_find_live_session_id_filters_by_started_at() {
        // An old jsonl from a prior claude run in the same cwd must not be
        // picked up when started_at is more recent.
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let stale = project_dir.join("abandoned.jsonl");
        std::fs::write(&stale, "{}\n").unwrap();
        set_mtime(&stale, -3600);

        // Session started 60s ago — the hour-old file must be filtered.
        let started_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(60_000);

        let sid = find_live_session_id(
            Some(project_dir.as_path()),
            started_ms,
            &std::collections::HashSet::new(),
        );
        assert!(sid.is_none(), "expected None, got {:?}", sid);
    }

    #[test]
    fn test_find_live_session_id_empty_dir_returns_none() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        assert!(find_live_session_id(
            Some(project_dir.as_path()),
            0,
            &std::collections::HashSet::new()
        )
        .is_none());
    }

    #[test]
    fn test_find_live_session_id_excludes_claimed_sids() {
        // Cross-PID hijack guard: a sibling PID's jsonl (newest on disk)
        // must not be adopted when it's in the `excluded` set.
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let mine = project_dir.join("mine.jsonl");
        let siblings = project_dir.join("sibling.jsonl");
        std::fs::write(&mine, "{}\n").unwrap();
        std::fs::write(&siblings, "{}\n").unwrap();
        set_mtime(&mine, -60);
        set_mtime(&siblings, 0); // sibling is newer

        let started_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(120_000);

        let mut excluded = std::collections::HashSet::new();
        excluded.insert("sibling");
        let sid = find_live_session_id(Some(project_dir.as_path()), started_ms, &excluded);
        assert_eq!(sid.as_deref(), Some("mine"));
    }

    #[test]
    fn test_find_live_session_id_grace_window_boundary() {
        // The 5s grace window must accept a file whose mtime is slightly
        // before started_at (clock skew). One second before started_at is in;
        // ten seconds before is out.
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let within = project_dir.join("within-grace.jsonl");
        std::fs::write(&within, "{}\n").unwrap();
        set_mtime(&within, -1);

        let outside = project_dir.join("outside-grace.jsonl");
        std::fs::write(&outside, "{}\n").unwrap();
        set_mtime(&outside, -10);

        // started_at = now → 5s grace covers the -1s file but not the -10s one.
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let sid = find_live_session_id(
            Some(project_dir.as_path()),
            started_ms,
            &std::collections::HashSet::new(),
        );
        assert_eq!(sid.as_deref(), Some("within-grace"));
    }

    #[test]
    fn test_load_session_overrides_sid_after_clear() {
        // Reproduces issue #68: session file still points at the PID's initial
        // sessionId, but a newer transcript (from /clear) exists in the same
        // project dir. The loaded session must reflect the new sid + its
        // counters, not the stale one.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 7777;
        let old_sid = "old-sid-before-clear";
        let new_sid = "new-sid-after-clear";
        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, old_sid, &cwd);

        let old_transcript = write_transcript(&projects, &cwd, old_sid, "first");
        let new_transcript = write_transcript(&projects, &cwd, new_sid, "after clear");
        set_mtime(&old_transcript, -30);
        set_mtime(&new_transcript, 0);

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, new_sid);
        // Token counters must reflect the NEW transcript's contents, not
        // a stale/empty state. `write_transcript` produces one assistant
        // turn with input_tokens=12 → verify that flowed through.
        assert_eq!(sessions[0].total_input_tokens, 12);
    }

    #[test]
    fn test_load_session_overrides_sid_in_worktree_project_dir() {
        // Worktree parity: when the transcript dir doesn't match
        // encode_cwd_path(cwd), the live-sid lookup must still find the
        // new jsonl next to the old one in the actual worktree dir.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 8888;
        let old_sid = "worktree-old";
        let new_sid = "worktree-new";

        // Simulate a worktree session: transcripts in a dir that does NOT
        // match encode_cwd_path(cwd).
        let worktree_dir = projects.join("worktree-branch");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        let old_transcript = worktree_dir.join(format!("{}.jsonl", old_sid));
        let new_transcript = worktree_dir.join(format!("{}.jsonl", new_sid));
        let turn_line = r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"x"}}
{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":42,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}
"#;
        std::fs::write(&old_transcript, turn_line).unwrap();
        std::fs::write(&new_transcript, turn_line).unwrap();
        set_mtime(&old_transcript, -30);
        set_mtime(&new_transcript, 0);

        let session_path = sessions.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, old_sid, &cwd);

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(session_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, new_sid);
        assert_eq!(sessions[0].total_input_tokens, 42);
    }

    #[test]
    fn test_load_session_skips_override_when_multiple_pids_share_cwd() {
        // Cross-PID hijack guard: two claude PIDs in the same cwd must keep
        // their original session_ids even if newer jsonls exist, since we
        // can't tell from mtime alone which one owns each file.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("shared-repo");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid_a = 9001;
        let pid_b = 9002;
        let sid_a = "sid-a";
        let sid_b = "sid-b";
        let path_a = sessions.join(format!("{}.json", pid_a));
        let path_b = sessions.join(format!("{}.json", pid_b));
        write_session_file(&path_a, pid_a, sid_a, &cwd);
        write_session_file(&path_b, pid_b, sid_b, &cwd);

        // Both PIDs have their jsonls + a "mystery" newer one that neither
        // session file claims. Without the guard, both PIDs would race to
        // adopt it.
        write_transcript(&projects, &cwd, sid_a, "a");
        write_transcript(&projects, &cwd, sid_b, "b");
        let mystery =
            write_transcript(&projects, &cwd, "newer-jsonl-someone-cleared", "mystery");
        set_mtime(&mystery, 0);

        let config = ConfigDir::new(profile.clone());
        let mut process_info = make_proc_info(pid_a, "claude");
        process_info.insert(
            pid_b,
            ProcInfo {
                pid: pid_b,
                ppid: 1,
                rss_kb: 2048,
                cpu_pct: 0.0,
                command: "claude".to_string(),
            },
        );
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        let session_paths = vec![(path_a, config.clone()), (path_b, config)];
        let ctx = build_discovery_context(&session_paths, &process_info);
        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        let sids: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert!(sids.contains(sid_a), "expected sid-a, got {:?}", sids);
        assert!(sids.contains(sid_b), "expected sid-b, got {:?}", sids);
        assert!(
            !sids.contains("newer-jsonl-someone-cleared"),
            "the mystery sid must not hijack either PID: {:?}",
            sids
        );
    }

    #[test]
    fn test_load_session_overrides_sid_despite_print_sibling() {
        // Regression guard: abtop spawns `claude --print` for summary
        // generation. Its `sessions/{PID}.json` lands in the same cwd as
        // the real session. If `build_discovery_context` counted those
        // spawns, `pids_per_cwd` would flip to 2 and the cross-PID guard
        // would silently suppress the /clear override on the real
        // session — re-introducing issue #68 on every machine running
        // abtop. Filter them out instead.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let real_pid = 7070;
        let print_pid = 7071;
        let real_old = "real-old";
        let real_new = "real-new";
        let print_sid = "print-spawn";

        let real_path = sessions_dir.join(format!("{}.json", real_pid));
        let print_path = sessions_dir.join(format!("{}.json", print_pid));
        write_session_file(&real_path, real_pid, real_old, &cwd);
        write_session_file(&print_path, print_pid, print_sid, &cwd);

        let old_transcript = write_transcript(&projects, &cwd, real_old, "first");
        let new_transcript = write_transcript(&projects, &cwd, real_new, "after clear");
        set_mtime(&old_transcript, -30);
        set_mtime(&new_transcript, 0);

        let config = ConfigDir::new(profile.clone());
        let mut process_info = make_proc_info(real_pid, "claude");
        process_info.insert(
            print_pid,
            ProcInfo {
                pid: print_pid,
                ppid: real_pid,
                rss_kb: 512,
                cpu_pct: 0.0,
                command: "claude --print -".to_string(),
            },
        );

        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];
        let session_paths = vec![(real_path, config.clone()), (print_path, config)];
        let ctx = build_discovery_context(&session_paths, &process_info);

        // The --print PID must not appear in the discovery context, so
        // `pids_per_cwd` stays at 1 and the override fires.
        assert_eq!(
            ctx.pids_per_cwd.get(cwd.to_str().unwrap()).copied(),
            Some(1),
            "--print sibling must not inflate pids_per_cwd",
        );
        assert!(
            !ctx.claimed_sids_by_pid.contains_key(&print_pid),
            "--print PID must not claim a sid",
        );

        let sessions = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );

        // Only the real session survives (--print is dropped in
        // load_session); its sid must be the post-/clear one.
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, real_new);
    }

    #[test]
    fn test_transcript_cache_evicts_old_sid_after_clear() {
        // Two-poll regression: on the first tick abtop parses the old
        // transcript and caches its counters under `old_sid`. After
        // `/clear`, a new `new_sid.jsonl` appears while `old_sid.jsonl`
        // lingers on disk. The second tick must (1) switch the session
        // to `new_sid`, (2) report ONLY the new transcript's counters —
        // not the sum of both — and (3) drop the stale `old_sid` entry
        // from `transcript_cache` so it can't leak forward.
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".claude");
        let sessions_dir = profile.join("sessions");
        let projects = profile.join("projects");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let pid = 6565;
        let old_sid = "pre-clear-sid";
        let new_sid = "post-clear-sid";
        let session_path = sessions_dir.join(format!("{}.json", pid));
        write_session_file(&session_path, pid, old_sid, &cwd);

        let old_transcript = write_transcript(&projects, &cwd, old_sid, "first chat");

        let config = ConfigDir::new(profile.clone());
        let process_info = make_proc_info(pid, "claude");
        let mut collector = ClaudeCollector::new();
        collector.config_dirs = vec![config.clone()];

        // Poll 1 — only old_sid exists.
        let session_paths = vec![(session_path.clone(), config.clone())];
        let ctx = build_discovery_context(&session_paths, &process_info);
        let first = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx,
        );
        collector.evict_stale_cache(&first);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].session_id, old_sid);
        assert_eq!(first[0].total_input_tokens, 12);
        assert!(
            collector.transcript_cache.contains_key(old_sid),
            "poll 1 should have cached the old sid",
        );

        // Simulate /clear: old jsonl is now older than the new one,
        // which appears with the same 12-token turn. If cache eviction
        // fails and counters leak, total_input_tokens would become 24.
        set_mtime(&old_transcript, -30);
        let new_transcript = {
            let dir = projects.join(encode_cwd_path(cwd.to_str().unwrap()));
            let p = dir.join(format!("{}.jsonl", new_sid));
            std::fs::write(
                &p,
                r#"{"type":"user","timestamp":"2026-03-28T15:10:00Z","message":{"role":"user","content":"second chat"}}
{"type":"assistant","timestamp":"2026-03-28T15:10:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":12,"output_tokens":6,"cache_read_input_tokens":3,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"done"}]}}
"#,
            )
            .unwrap();
            p
        };
        set_mtime(&new_transcript, 0);

        // Poll 2 — override must fire and old cache entry must drop.
        let ctx2 = build_discovery_context(&session_paths, &process_info);
        let second = collector.load_session_paths(
            &session_paths,
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &ctx2,
        );
        collector.evict_stale_cache(&second);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].session_id, new_sid);
        assert_eq!(
            second[0].total_input_tokens, 12,
            "counters must reflect only the new transcript, not old+new",
        );
        assert!(
            !collector.transcript_cache.contains_key(old_sid),
            "stale old sid must be evicted after /clear",
        );
        assert!(
            collector.transcript_cache.contains_key(new_sid),
            "new sid must be present in the cache after poll 2",
        );
    }
}
