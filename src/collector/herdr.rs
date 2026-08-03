//! Supplemental, content-free Codex status from Herdr-owned terminals.

use super::process;
use crate::herdr::{self, StatusAgentPane, HERDR_ENV, HERDR_SOCKET_PATH};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_SOCKETS: usize = 8;
const MAX_AGENT_ROWS: usize = 256;
const MAX_PANE_PROBES: usize = 32;
const MAX_SOCKET_BYTES: usize = 4096;
const MAX_ID_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HerdrStatus {
    Blocked,
    Working,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrTarget {
    pub session_id: String,
    pub pid: u32,
    pub expected_incarnation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HerdrObservation {
    pub status: HerdrStatus,
    pub observed_at_ms: u64,
    pub status_since_ms: u64,
    pub consecutive_matching: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TargetKey {
    session_id: String,
    pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContinuityKey {
    socket_path: String,
    terminal_id: String,
    pane_id: String,
    session_id: String,
    pid: u32,
    process_incarnation: String,
    status: HerdrStatus,
    state_change_seq: u64,
}

#[derive(Debug, Clone, Copy)]
struct Continuity {
    first_observed_at_ms: u64,
    consecutive_matching: u32,
}

#[derive(Debug, Clone)]
struct PreparedTarget {
    key: TargetKey,
    incarnation: String,
    sockets: Vec<String>,
}

#[derive(Debug, Clone)]
struct ValidatedMatch {
    socket_path: String,
    agent: StatusAgentPane,
    target: PreparedTarget,
    status: HerdrStatus,
}

fn agent_snapshot_identities_are_unique(agents: &[StatusAgentPane]) -> bool {
    let mut terminal_ids = HashSet::new();
    let mut pane_ids = HashSet::new();
    agents.iter().all(|agent| {
        terminal_ids.insert(agent.terminal_id.as_str()) && pane_ids.insert(agent.pane_id.as_str())
    })
}

fn globally_unique_match_keys(
    matches: &HashMap<TargetKey, Vec<ValidatedMatch>>,
) -> HashSet<TargetKey> {
    let mut session_match_counts = HashMap::<String, usize>::new();
    let mut process_match_counts = HashMap::<(u32, String), usize>::new();
    let mut terminal_match_counts = HashMap::<(String, String), usize>::new();
    let mut pane_match_counts = HashMap::<(String, String), usize>::new();
    for candidates in matches.values() {
        for candidate in candidates {
            *session_match_counts
                .entry(candidate.target.key.session_id.clone())
                .or_default() += 1;
            *process_match_counts
                .entry((
                    candidate.target.key.pid,
                    candidate.target.incarnation.clone(),
                ))
                .or_default() += 1;
            *terminal_match_counts
                .entry((
                    candidate.socket_path.clone(),
                    candidate.agent.terminal_id.clone(),
                ))
                .or_default() += 1;
            *pane_match_counts
                .entry((
                    candidate.socket_path.clone(),
                    candidate.agent.pane_id.clone(),
                ))
                .or_default() += 1;
        }
    }

    matches
        .iter()
        .filter_map(|(target, candidates)| {
            let [candidate] = candidates.as_slice() else {
                return None;
            };
            (session_match_counts.get(&target.session_id) == Some(&1)
                && process_match_counts.get(&(
                    candidate.target.key.pid,
                    candidate.target.incarnation.clone(),
                )) == Some(&1)
                && terminal_match_counts.get(&(
                    candidate.socket_path.clone(),
                    candidate.agent.terminal_id.clone(),
                )) == Some(&1)
                && pane_match_counts.get(&(
                    candidate.socket_path.clone(),
                    candidate.agent.pane_id.clone(),
                )) == Some(&1))
            .then(|| target.clone())
        })
        .collect()
}

trait ResolverProbe {
    fn current_socket(&mut self) -> Option<String>;
    fn process_incarnation(&mut self, pid: u32) -> Option<String>;
    fn native_codex_process_is_exact(&mut self, pid: u32, incarnation: &str) -> bool;
    fn process_env_var(&mut self, pid: u32, name: &str) -> Option<String>;
    fn run(
        &mut self,
        socket_path: &str,
        args: &[&str],
        deadline: Instant,
    ) -> Option<herdr::CommandOutput>;
}

struct SystemProbe;

impl ResolverProbe for SystemProbe {
    fn current_socket(&mut self) -> Option<String> {
        current_socket()
    }

    fn process_incarnation(&mut self, pid: u32) -> Option<String> {
        process::get_process_incarnation(pid)
    }

    fn native_codex_process_is_exact(&mut self, pid: u32, incarnation: &str) -> bool {
        super::codex::native_codex_process_is_exact(pid, incarnation)
    }

    fn process_env_var(&mut self, pid: u32, name: &str) -> Option<String> {
        process::read_process_env_var(pid, name)
    }

    fn run(
        &mut self,
        socket_path: &str,
        args: &[&str],
        deadline: Instant,
    ) -> Option<herdr::CommandOutput> {
        run(socket_path, args, deadline)
    }
}

#[derive(Default)]
pub(crate) struct HerdrStatusResolver {
    continuity: HashMap<ContinuityKey, Continuity>,
    #[cfg(test)]
    test_probe: Option<Box<dyn ResolverProbe>>,
}

impl HerdrStatusResolver {
    pub(crate) fn resolve(
        &mut self,
        targets: &[HerdrTarget],
        now_ms: u64,
    ) -> HashMap<(String, u32), HerdrObservation> {
        #[cfg(test)]
        if let Some(mut probe) = self.test_probe.take() {
            let observations = self.resolve_with_probe(targets, now_ms, probe.as_mut());
            self.test_probe = Some(probe);
            return observations;
        }

        self.resolve_with_probe(targets, now_ms, &mut SystemProbe)
    }

    fn resolve_with_probe<P: ResolverProbe + ?Sized>(
        &mut self,
        targets: &[HerdrTarget],
        now_ms: u64,
        probe: &mut P,
    ) -> HashMap<(String, u32), HerdrObservation> {
        let current_socket = probe.current_socket();
        let mut prepared = Vec::new();
        let mut sockets = HashSet::new();
        let mut invalid_session_ids = HashSet::new();
        for target in targets {
            let Some(candidate) = prepare_target(target, current_socket.as_deref(), probe) else {
                if !target.session_id.is_empty() && target.session_id.len() <= MAX_ID_BYTES {
                    invalid_session_ids.insert(target.session_id.clone());
                }
                continue;
            };
            sockets.extend(candidate.sockets.iter().cloned());
            prepared.push(candidate);
        }
        if sockets.is_empty() || sockets.len() > MAX_SOCKETS {
            self.continuity.clear();
            return HashMap::new();
        }

        let deadline = Instant::now() + TOTAL_TIMEOUT;
        let mut matches = HashMap::<TargetKey, Vec<ValidatedMatch>>::new();
        let mut invalid_targets = HashSet::<TargetKey>::new();
        let mut probes = 0_usize;
        let mut sockets = sockets.into_iter().collect::<Vec<_>>();
        sockets.sort();
        for socket_path in sockets {
            let socket_targets = prepared
                .iter()
                .filter(|target| target.sockets.contains(&socket_path))
                .collect::<Vec<_>>();
            let Some(first) = list_agents(probe, &socket_path, deadline) else {
                invalid_targets.extend(socket_targets.iter().map(|target| target.key.clone()));
                continue;
            };
            if first.len() > MAX_AGENT_ROWS {
                invalid_targets.extend(socket_targets.iter().map(|target| target.key.clone()));
                continue;
            }
            if !agent_snapshot_identities_are_unique(&first) {
                invalid_targets.extend(socket_targets.iter().map(|target| target.key.clone()));
                continue;
            }

            let mut candidates = Vec::new();
            for target in socket_targets {
                let matching = first
                    .iter()
                    .filter(|agent| agent_matches_target(agent, target))
                    .collect::<Vec<_>>();
                match matching.as_slice() {
                    [] => {}
                    [agent] => candidates.push(((*target).clone(), (*agent).clone())),
                    _ => {
                        invalid_targets.insert(target.key.clone());
                    }
                }
            }
            if probes.saturating_add(candidates.len()) > MAX_PANE_PROBES {
                self.continuity.clear();
                return HashMap::new();
            }

            let mut owned = Vec::new();
            for (target, agent) in candidates {
                probes += 1;
                let Some(info) = process_info(probe, &socket_path, &agent.pane_id, deadline) else {
                    invalid_targets.insert(target.key.clone());
                    continue;
                };
                if !process_info_owns_pid(&info, &agent.pane_id, target.key.pid) {
                    invalid_targets.insert(target.key.clone());
                    continue;
                }
                owned.push((target, agent));
            }
            if owned.is_empty() {
                continue;
            }

            let Some(second) = list_agents(probe, &socket_path, deadline) else {
                invalid_targets.extend(owned.iter().map(|(target, _)| target.key.clone()));
                continue;
            };
            if second.len() > MAX_AGENT_ROWS || !agent_snapshots_unchanged(&first, &second) {
                invalid_targets.extend(owned.iter().map(|(target, _)| target.key.clone()));
                continue;
            }
            for (target, agent) in owned {
                if probe.process_incarnation(target.key.pid).as_deref()
                    != Some(target.incarnation.as_str())
                    || !probe.native_codex_process_is_exact(target.key.pid, &target.incarnation)
                {
                    invalid_targets.insert(target.key.clone());
                    continue;
                }
                let Some(status) = parse_status(&agent.agent_status) else {
                    invalid_targets.insert(target.key.clone());
                    continue;
                };
                matches
                    .entry(target.key.clone())
                    .or_default()
                    .push(ValidatedMatch {
                        socket_path: socket_path.clone(),
                        agent,
                        target,
                        status,
                    });
            }
        }
        invalid_session_ids.extend(
            invalid_targets
                .iter()
                .map(|target| target.session_id.clone()),
        );
        matches.retain(|key, _| !invalid_session_ids.contains(&key.session_id));

        let unique_targets = globally_unique_match_keys(&matches);

        let mut current_keys = HashSet::new();
        let mut observations = HashMap::new();
        for (target_key, candidates) in matches {
            let [candidate] = candidates.as_slice() else {
                continue;
            };
            if !unique_targets.contains(&target_key) {
                continue;
            }
            let continuity_key = ContinuityKey {
                socket_path: candidate.socket_path.clone(),
                terminal_id: candidate.agent.terminal_id.clone(),
                pane_id: candidate.agent.pane_id.clone(),
                session_id: candidate.target.key.session_id.clone(),
                pid: candidate.target.key.pid,
                process_incarnation: candidate.target.incarnation.clone(),
                status: candidate.status,
                state_change_seq: candidate.agent.state_change_seq,
            };
            current_keys.insert(continuity_key.clone());
            let continuity = self
                .continuity
                .entry(continuity_key)
                .and_modify(|entry| {
                    entry.consecutive_matching =
                        entry.consecutive_matching.saturating_add(1).max(1);
                })
                .or_insert(Continuity {
                    first_observed_at_ms: now_ms,
                    consecutive_matching: 1,
                });
            observations.insert(
                (target_key.session_id, target_key.pid),
                HerdrObservation {
                    status: candidate.status,
                    observed_at_ms: now_ms,
                    status_since_ms: continuity.first_observed_at_ms,
                    consecutive_matching: continuity.consecutive_matching,
                },
            );
        }
        self.continuity.retain(|key, _| current_keys.contains(key));
        observations
    }

    #[cfg(test)]
    fn set_test_probe(&mut self, probe: impl ResolverProbe + 'static) {
        self.test_probe = Some(Box::new(probe));
    }
}

fn prepare_target<P: ResolverProbe + ?Sized>(
    target: &HerdrTarget,
    current_socket: Option<&str>,
    probe: &mut P,
) -> Option<PreparedTarget> {
    if target.pid == 0 || target.session_id.is_empty() || target.session_id.len() > MAX_ID_BYTES {
        return None;
    }
    let before = probe.process_incarnation(target.pid)?;
    if target
        .expected_incarnation
        .as_deref()
        .is_some_and(|expected| expected != before)
    {
        return None;
    }
    let target_is_herdr = probe.process_env_var(target.pid, HERDR_ENV).as_deref() == Some("1");
    let target_socket = target_is_herdr
        .then(|| probe.process_env_var(target.pid, HERDR_SOCKET_PATH))
        .flatten()
        .filter(|socket| valid_socket(socket));
    if probe.process_incarnation(target.pid).as_deref() != Some(before.as_str()) {
        return None;
    }
    let mut sockets = Vec::new();
    if let Some(socket) = target_socket {
        sockets.push(socket);
    }
    if let Some(socket) = current_socket.filter(|socket| valid_socket(socket)) {
        if !sockets.iter().any(|candidate| candidate == socket) {
            sockets.push(socket.to_string());
        }
    }
    (!sockets.is_empty()).then(|| PreparedTarget {
        key: TargetKey {
            session_id: target.session_id.clone(),
            pid: target.pid,
        },
        incarnation: before,
        sockets,
    })
}

fn current_socket() -> Option<String> {
    (std::env::var(HERDR_ENV).ok().as_deref() == Some("1"))
        .then(|| std::env::var(HERDR_SOCKET_PATH).ok())
        .flatten()
        .filter(|socket| valid_socket(socket))
}

fn valid_socket(socket: &str) -> bool {
    !socket.is_empty()
        && socket.len() <= MAX_SOCKET_BYTES
        && !socket
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
        && Path::new(socket).is_absolute()
}

fn agent_matches_target(agent: &StatusAgentPane, target: &PreparedTarget) -> bool {
    if agent.screen_detection_skipped
        || agent.terminal_id.is_empty()
        || agent.terminal_id.len() > MAX_ID_BYTES
        || agent.pane_id.is_empty()
        || agent.pane_id.len() > MAX_ID_BYTES
        || agent.agent.as_deref() != Some("codex")
    {
        return false;
    }
    agent.agent_session.as_ref().is_some_and(|session| {
        session.source == "herdr:codex"
            && session.agent == "codex"
            && session.kind == "id"
            && session.value == target.key.session_id
            && session.source.len() <= MAX_ID_BYTES
            && session.agent.len() <= MAX_ID_BYTES
            && session.kind.len() <= MAX_ID_BYTES
            && session.value.len() <= MAX_ID_BYTES
    })
}

fn process_info_owns_pid(info: &herdr::ProcessInfo, pane_id: &str, pid: u32) -> bool {
    info.pane_id == pane_id
        && info.foreground_processes.len() <= MAX_AGENT_ROWS
        && info
            .foreground_processes
            .iter()
            .any(|process| process.pid == pid)
}

fn agent_snapshots_unchanged(first: &[StatusAgentPane], second: &[StatusAgentPane]) -> bool {
    if first.len() != second.len() {
        return false;
    }
    let mut first = first.to_vec();
    let mut second = second.to_vec();
    first.sort_unstable();
    second.sort_unstable();
    first == second
}

fn parse_status(status: &str) -> Option<HerdrStatus> {
    match status {
        "blocked" => Some(HerdrStatus::Blocked),
        "working" => Some(HerdrStatus::Working),
        "idle" => Some(HerdrStatus::Idle),
        _ => None,
    }
}

fn list_agents<P: ResolverProbe + ?Sized>(
    probe: &mut P,
    socket_path: &str,
    deadline: Instant,
) -> Option<Vec<StatusAgentPane>> {
    let output = probe.run(socket_path, &["agent", "list"], deadline)?;
    output
        .success
        .then(|| herdr::parse_status_agent_list(&output.stdout).ok())
        .flatten()
}

fn process_info<P: ResolverProbe + ?Sized>(
    probe: &mut P,
    socket_path: &str,
    pane_id: &str,
    deadline: Instant,
) -> Option<herdr::ProcessInfo> {
    let output = probe.run(
        socket_path,
        &["pane", "process-info", "--pane", pane_id],
        deadline,
    )?;
    output
        .success
        .then(|| herdr::parse_process_info(&output.stdout).ok())
        .flatten()
}

fn run(socket_path: &str, args: &[&str], deadline: Instant) -> Option<herdr::CommandOutput> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    if remaining.is_zero() {
        return None;
    }
    let timeout = remaining.min(COMMAND_TIMEOUT);
    let args = args
        .iter()
        .map(|arg| (*arg).to_string())
        .collect::<Vec<_>>();
    herdr::run_herdr(socket_path, &args, timeout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const TEST_PID: u32 = 42;
    const TEST_SESSION: &str = "session-1";
    #[cfg(not(windows))]
    const TEST_SOCKET: &str = "/tmp/herdr.sock";
    #[cfg(windows)]
    const TEST_SOCKET: &str = r"C:\herdr.sock";
    const TEST_INCARNATION: &str = "incarnation-1";

    fn named_test_socket(name: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\{name}.sock")
        } else {
            format!("/tmp/{name}.sock")
        }
    }

    struct FakeProbe {
        current_socket: Option<String>,
        target_socket: Option<String>,
        fallback_incarnation: Option<String>,
        incarnation_script: VecDeque<Option<String>>,
        native_exact: bool,
        commands: VecDeque<(String, Vec<String>, Option<herdr::CommandOutput>)>,
    }

    impl FakeProbe {
        fn new(commands: Vec<(Vec<String>, Option<herdr::CommandOutput>)>) -> Self {
            Self::new_on(
                commands
                    .into_iter()
                    .map(|(args, output)| (TEST_SOCKET.to_string(), args, output))
                    .collect(),
            )
        }

        fn new_on(commands: Vec<(String, Vec<String>, Option<herdr::CommandOutput>)>) -> Self {
            Self {
                current_socket: None,
                target_socket: Some(TEST_SOCKET.to_string()),
                fallback_incarnation: Some(TEST_INCARNATION.to_string()),
                incarnation_script: VecDeque::new(),
                native_exact: true,
                commands: commands.into(),
            }
        }

        fn stable(agent: &StatusAgentPane) -> Self {
            Self::new(vec![
                expected(
                    &["agent", "list"],
                    Some(agent_list_output(std::slice::from_ref(agent))),
                ),
                expected(
                    &["pane", "process-info", "--pane", &agent.pane_id],
                    Some(process_info_output(&agent.pane_id, &[TEST_PID])),
                ),
                expected(
                    &["agent", "list"],
                    Some(agent_list_output(std::slice::from_ref(agent))),
                ),
            ])
        }
    }

    impl ResolverProbe for FakeProbe {
        fn current_socket(&mut self) -> Option<String> {
            self.current_socket.clone()
        }

        fn process_incarnation(&mut self, pid: u32) -> Option<String> {
            assert_eq!(pid, TEST_PID);
            self.incarnation_script
                .pop_front()
                .unwrap_or_else(|| self.fallback_incarnation.clone())
        }

        fn native_codex_process_is_exact(&mut self, pid: u32, incarnation: &str) -> bool {
            assert_eq!(pid, TEST_PID);
            assert_eq!(incarnation, TEST_INCARNATION);
            self.native_exact
        }

        fn process_env_var(&mut self, pid: u32, name: &str) -> Option<String> {
            assert_eq!(pid, TEST_PID);
            match name {
                HERDR_ENV if self.target_socket.is_some() => Some("1".to_string()),
                HERDR_SOCKET_PATH => self.target_socket.clone(),
                _ => None,
            }
        }

        fn run(
            &mut self,
            socket_path: &str,
            args: &[&str],
            _deadline: Instant,
        ) -> Option<herdr::CommandOutput> {
            let (expected_socket, expected_args, output) =
                self.commands.pop_front().expect("unexpected Herdr command");
            assert_eq!(socket_path, expected_socket);
            assert_eq!(
                args.iter()
                    .map(|arg| (*arg).to_string())
                    .collect::<Vec<_>>(),
                expected_args
            );
            output
        }
    }

    fn expected(
        args: &[&str],
        output: Option<herdr::CommandOutput>,
    ) -> (Vec<String>, Option<herdr::CommandOutput>) {
        (args.iter().map(|arg| (*arg).to_string()).collect(), output)
    }

    fn expected_on(
        socket: &str,
        args: &[&str],
        output: Option<herdr::CommandOutput>,
    ) -> (String, Vec<String>, Option<herdr::CommandOutput>) {
        (
            socket.to_string(),
            args.iter().map(|arg| (*arg).to_string()).collect(),
            output,
        )
    }

    fn output(stdout: Vec<u8>) -> herdr::CommandOutput {
        herdr::CommandOutput {
            success: true,
            status: "exit status: 0".to_string(),
            stdout,
            stderr: Vec::new(),
        }
    }

    fn agent_list_output(agents: &[StatusAgentPane]) -> herdr::CommandOutput {
        let agents = agents
            .iter()
            .map(|agent| {
                let session = agent.agent_session.as_ref().map(|session| {
                    serde_json::json!({
                        "source": session.source,
                        "agent": session.agent,
                        "kind": session.kind,
                        "value": session.value,
                    })
                });
                let mut row = serde_json::json!({
                    "terminal_id": agent.terminal_id,
                    "agent": agent.agent,
                    "agent_status": agent.agent_status,
                    "agent_session": session,
                    "pane_id": agent.pane_id,
                    "state_change_seq": agent.state_change_seq,
                    "revision": agent.revision,
                });
                if agent.screen_detection_skipped {
                    row["screen_detection_skipped"] = serde_json::Value::Bool(true);
                }
                row
            })
            .collect::<Vec<_>>();
        output(
            serde_json::json!({"result": {"agents": agents}})
                .to_string()
                .into_bytes(),
        )
    }

    fn process_info_output(pane_id: &str, pids: &[u32]) -> herdr::CommandOutput {
        let processes = pids
            .iter()
            .map(|pid| serde_json::json!({"pid": pid}))
            .collect::<Vec<_>>();
        output(
            serde_json::json!({
                "result": {
                    "process_info": {
                        "pane_id": pane_id,
                        "foreground_processes": processes,
                    }
                }
            })
            .to_string()
            .into_bytes(),
        )
    }

    fn target() -> HerdrTarget {
        HerdrTarget {
            session_id: TEST_SESSION.to_string(),
            pid: TEST_PID,
            expected_incarnation: Some(TEST_INCARNATION.to_string()),
        }
    }

    fn resolve_stable(
        resolver: &mut HerdrStatusResolver,
        agent: &StatusAgentPane,
        now_ms: u64,
    ) -> HerdrObservation {
        let observations = resolve_with_fake(resolver, now_ms, FakeProbe::stable(agent));
        *observations
            .get(&(TEST_SESSION.to_string(), TEST_PID))
            .expect("stable exact match should resolve")
    }

    fn resolve_with_fake(
        resolver: &mut HerdrStatusResolver,
        now_ms: u64,
        probe: FakeProbe,
    ) -> HashMap<(String, u32), HerdrObservation> {
        resolver.set_test_probe(probe);
        resolver.resolve(&[target()], now_ms)
    }

    fn assert_continuity_restarts(
        resolver: &mut HerdrStatusResolver,
        agent: &StatusAgentPane,
        now_ms: u64,
    ) {
        let observation = resolve_stable(resolver, agent, now_ms);
        assert_eq!(observation.status_since_ms, now_ms);
        assert_eq!(observation.consecutive_matching, 1);
    }

    fn pane(status: &str, session_id: &str) -> StatusAgentPane {
        StatusAgentPane {
            terminal_id: "terminal-1".to_string(),
            agent: Some("codex".to_string()),
            agent_status: status.to_string(),
            screen_detection_skipped: false,
            agent_session: Some(crate::herdr::AgentSessionInfo {
                source: "herdr:codex".to_string(),
                agent: "codex".to_string(),
                kind: "id".to_string(),
                value: session_id.to_string(),
            }),
            pane_id: "pane-1".to_string(),
            state_change_seq: 7,
            revision: 9,
        }
    }

    #[test]
    fn resolve_accepts_a_stable_exact_match_and_tracks_continuity() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);

        let first = resolve_stable(&mut resolver, &agent, 100);
        assert_eq!(
            first,
            HerdrObservation {
                status: HerdrStatus::Working,
                observed_at_ms: 100,
                status_since_ms: 100,
                consecutive_matching: 1,
            }
        );

        let second = resolve_stable(&mut resolver, &agent, 125);
        assert_eq!(second.observed_at_ms, 125);
        assert_eq!(second.status_since_ms, 100);
        assert_eq!(second.consecutive_matching, 2);
    }

    #[test]
    fn resolve_rejects_a_changed_snapshot_and_clears_continuity() {
        let mut resolver = HerdrStatusResolver::default();
        let first = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &first, 100);

        let mut changed = first.clone();
        changed.state_change_seq += 1;
        let probe = FakeProbe::new(vec![
            expected(
                &["agent", "list"],
                Some(agent_list_output(std::slice::from_ref(&first))),
            ),
            expected(
                &["pane", "process-info", "--pane", &first.pane_id],
                Some(process_info_output(&first.pane_id, &[TEST_PID])),
            ),
            expected(
                &["agent", "list"],
                Some(agent_list_output(std::slice::from_ref(&changed))),
            ),
        ]);
        assert!(resolve_with_fake(&mut resolver, 125, probe).is_empty());

        assert_continuity_restarts(&mut resolver, &changed, 150);
    }

    #[test]
    fn resolve_requires_the_entire_agent_snapshot_to_remain_unchanged() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &agent, 100);

        let mut unrelated = pane("idle", "another-session");
        unrelated.pane_id = "pane-2".to_string();
        unrelated.terminal_id = "terminal-2".to_string();
        let mut changed_unrelated = unrelated.clone();
        changed_unrelated.state_change_seq += 1;
        let probe = FakeProbe::new(vec![
            expected(
                &["agent", "list"],
                Some(agent_list_output(&[agent.clone(), unrelated])),
            ),
            expected(
                &["pane", "process-info", "--pane", &agent.pane_id],
                Some(process_info_output(&agent.pane_id, &[TEST_PID])),
            ),
            expected(
                &["agent", "list"],
                Some(agent_list_output(&[agent.clone(), changed_unrelated])),
            ),
        ]);
        assert!(resolve_with_fake(&mut resolver, 125, probe).is_empty());

        assert_continuity_restarts(&mut resolver, &agent, 150);
    }

    #[test]
    fn resolve_accepts_a_semantically_unchanged_reordered_snapshot() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);
        let mut unrelated = pane("idle", "another-session");
        unrelated.pane_id = "pane-2".to_string();
        unrelated.terminal_id = "terminal-2".to_string();
        let probe = FakeProbe::new(vec![
            expected(
                &["agent", "list"],
                Some(agent_list_output(&[agent.clone(), unrelated.clone()])),
            ),
            expected(
                &["pane", "process-info", "--pane", &agent.pane_id],
                Some(process_info_output(&agent.pane_id, &[TEST_PID])),
            ),
            expected(
                &["agent", "list"],
                Some(agent_list_output(&[unrelated, agent.clone()])),
            ),
        ]);

        let observations = resolve_with_fake(&mut resolver, 100, probe);
        assert_eq!(
            observations
                .get(&(TEST_SESSION.to_string(), TEST_PID))
                .map(|observation| observation.status),
            Some(HerdrStatus::Working)
        );
    }

    #[test]
    fn resolve_rejects_duplicate_session_claims_and_clears_continuity() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &agent, 100);

        let probe = FakeProbe::new(vec![expected(
            &["agent", "list"],
            Some(agent_list_output(&[agent.clone(), agent.clone()])),
        )]);
        assert!(resolve_with_fake(&mut resolver, 125, probe).is_empty());

        assert_continuity_restarts(&mut resolver, &agent, 150);
    }

    #[test]
    fn resolve_rejects_target_and_unrelated_duplicate_before_probing_and_clears_continuity() {
        struct DuplicateClaimProbe {
            incarnations: HashMap<u32, String>,
            list_output: Option<herdr::CommandOutput>,
        }

        impl ResolverProbe for DuplicateClaimProbe {
            fn current_socket(&mut self) -> Option<String> {
                None
            }

            fn process_incarnation(&mut self, pid: u32) -> Option<String> {
                self.incarnations.get(&pid).cloned()
            }

            fn native_codex_process_is_exact(&mut self, _: u32, _: &str) -> bool {
                panic!("ambiguous pane claims must be rejected before process validation")
            }

            fn process_env_var(&mut self, _: u32, name: &str) -> Option<String> {
                match name {
                    HERDR_ENV => Some("1".to_string()),
                    HERDR_SOCKET_PATH => Some(TEST_SOCKET.to_string()),
                    _ => None,
                }
            }

            fn run(
                &mut self,
                socket_path: &str,
                args: &[&str],
                _: Instant,
            ) -> Option<herdr::CommandOutput> {
                assert_eq!(socket_path, TEST_SOCKET);
                assert_eq!(args, ["agent", "list"]);
                self.list_output.take()
            }
        }

        let mut resolver = HerdrStatusResolver::default();
        let first = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &first, 100);

        let mut unrelated = pane("idle", "unrelated-session");
        unrelated.agent = Some("claude".to_string());
        unrelated.agent_session = Some(crate::herdr::AgentSessionInfo {
            source: "herdr:claude".to_string(),
            agent: "claude".to_string(),
            kind: "id".to_string(),
            value: "unrelated-session".to_string(),
        });
        unrelated.terminal_id = "terminal-unrelated".to_string();
        resolver.set_test_probe(DuplicateClaimProbe {
            incarnations: HashMap::from([(TEST_PID, TEST_INCARNATION.to_string())]),
            list_output: Some(agent_list_output(&[first.clone(), unrelated])),
        });
        assert!(resolver.resolve(&[target()], 125).is_empty());

        assert_continuity_restarts(&mut resolver, &first, 150);
    }

    #[test]
    fn resolve_rejects_unrelated_terminal_duplicate_before_probing_and_clears_continuity() {
        let mut resolver = HerdrStatusResolver::default();
        let first = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &first, 100);

        let mut unrelated = pane("idle", "unrelated-session");
        unrelated.agent = Some("claude".to_string());
        unrelated.agent_session = Some(crate::herdr::AgentSessionInfo {
            source: "herdr:claude".to_string(),
            agent: "claude".to_string(),
            kind: "id".to_string(),
            value: "unrelated-session".to_string(),
        });
        unrelated.pane_id = "pane-unrelated".to_string();
        assert_eq!(unrelated.terminal_id, first.terminal_id);

        let probe = FakeProbe::new(vec![expected(
            &["agent", "list"],
            Some(agent_list_output(&[first.clone(), unrelated])),
        )]);
        assert!(resolve_with_fake(&mut resolver, 125, probe).is_empty());

        assert_continuity_restarts(&mut resolver, &first, 150);
    }

    #[test]
    fn resolve_rejects_timeout_and_malformed_probes_and_clears_continuity() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &agent, 100);

        let timeout = FakeProbe::new(vec![expected(&["agent", "list"], None)]);
        assert!(resolve_with_fake(&mut resolver, 125, timeout).is_empty());
        assert_continuity_restarts(&mut resolver, &agent, 150);

        let malformed_output = output(b"{not-json".to_vec());
        let malformed = FakeProbe::new(vec![expected(&["agent", "list"], Some(malformed_output))]);
        assert!(resolve_with_fake(&mut resolver, 175, malformed).is_empty());
        assert_continuity_restarts(&mut resolver, &agent, 200);
    }

    #[test]
    fn resolve_rejects_partial_success_across_candidate_sockets() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &agent, 100);
        let current_socket = named_test_socket("current-herdr");

        let mut probe = FakeProbe::new_on(vec![
            expected_on(&current_socket, &["agent", "list"], None),
            expected_on(
                TEST_SOCKET,
                &["agent", "list"],
                Some(agent_list_output(std::slice::from_ref(&agent))),
            ),
            expected_on(
                TEST_SOCKET,
                &["pane", "process-info", "--pane", &agent.pane_id],
                Some(process_info_output(&agent.pane_id, &[TEST_PID])),
            ),
            expected_on(
                TEST_SOCKET,
                &["agent", "list"],
                Some(agent_list_output(std::slice::from_ref(&agent))),
            ),
        ]);
        probe.current_socket = Some(current_socket);
        assert!(resolve_with_fake(&mut resolver, 125, probe).is_empty());

        assert_continuity_restarts(&mut resolver, &agent, 150);
    }

    #[test]
    fn resolve_rejects_missing_exact_pid_and_clears_continuity() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &agent, 100);

        let probe = FakeProbe::new(vec![
            expected(
                &["agent", "list"],
                Some(agent_list_output(std::slice::from_ref(&agent))),
            ),
            expected(
                &["pane", "process-info", "--pane", &agent.pane_id],
                Some(process_info_output(&agent.pane_id, &[TEST_PID + 1])),
            ),
        ]);
        assert!(resolve_with_fake(&mut resolver, 125, probe).is_empty());

        assert_continuity_restarts(&mut resolver, &agent, 150);
    }

    #[test]
    fn resolve_rejects_pid_incarnation_change_and_clears_continuity() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &agent, 100);

        let mut probe = FakeProbe::stable(&agent);
        probe.incarnation_script = VecDeque::from([
            Some(TEST_INCARNATION.to_string()),
            Some(TEST_INCARNATION.to_string()),
            Some("incarnation-2".to_string()),
        ]);
        assert!(resolve_with_fake(&mut resolver, 125, probe).is_empty());

        assert_continuity_restarts(&mut resolver, &agent, 150);
    }

    #[test]
    fn resolve_rechecks_the_native_codex_process_and_clears_continuity() {
        let mut resolver = HerdrStatusResolver::default();
        let agent = pane("working", TEST_SESSION);
        resolve_stable(&mut resolver, &agent, 100);

        let mut probe = FakeProbe::stable(&agent);
        probe.native_exact = false;
        assert!(resolve_with_fake(&mut resolver, 125, probe).is_empty());

        assert_continuity_restarts(&mut resolver, &agent, 150);
    }

    #[test]
    fn matching_requires_exact_codex_session_identity() {
        let target = PreparedTarget {
            key: TargetKey {
                session_id: "session-1".to_string(),
                pid: 42,
            },
            incarnation: "incarnation".to_string(),
            sockets: vec![TEST_SOCKET.to_string()],
        };
        assert!(agent_matches_target(&pane("blocked", "session-1"), &target));
        assert!(!agent_matches_target(
            &pane("blocked", "session-2"),
            &target
        ));

        let mut skipped = pane("blocked", "session-1");
        skipped.screen_detection_skipped = true;
        assert!(!agent_matches_target(&skipped, &target));
    }

    #[test]
    fn only_screen_states_with_useful_semantics_are_accepted() {
        assert_eq!(parse_status("blocked"), Some(HerdrStatus::Blocked));
        assert_eq!(parse_status("working"), Some(HerdrStatus::Working));
        assert_eq!(parse_status("idle"), Some(HerdrStatus::Idle));
        assert_eq!(parse_status("done"), None);
        assert_eq!(parse_status("unknown"), None);
    }

    #[test]
    fn duplicate_session_claims_and_changed_snapshots_are_detectable() {
        let target = PreparedTarget {
            key: TargetKey {
                session_id: "session-1".to_string(),
                pid: 42,
            },
            incarnation: "incarnation".to_string(),
            sockets: vec![TEST_SOCKET.to_string()],
        };
        let first = pane("working", "session-1");
        assert_eq!(
            std::slice::from_ref(&first)
                .iter()
                .filter(|agent| agent_matches_target(agent, &target))
                .count(),
            1
        );
        assert_eq!(
            [first.clone(), first.clone()]
                .iter()
                .filter(|agent| agent_matches_target(agent, &target))
                .count(),
            2
        );

        let mut changed = first.clone();
        changed.state_change_seq += 1;
        assert_ne!(vec![changed], vec![first.clone()]);
        assert_eq!(vec![first.clone()], vec![first]);
    }

    #[test]
    fn validated_matches_are_unique_by_session_process_terminal_and_pane_globally() {
        let candidate = |session_id: &str, pid: u32, incarnation: &str, pane_id: &str| {
            let mut agent = pane("working", session_id);
            agent.pane_id = pane_id.to_string();
            agent.terminal_id = format!("terminal-{pane_id}");
            ValidatedMatch {
                socket_path: named_test_socket(pane_id),
                agent,
                target: PreparedTarget {
                    key: TargetKey {
                        session_id: session_id.to_string(),
                        pid,
                    },
                    incarnation: incarnation.to_string(),
                    sockets: Vec::new(),
                },
                status: HerdrStatus::Working,
            }
        };

        let one = candidate("session-1", 42, "incarnation-1", "pane-1");
        let one_key = one.target.key.clone();
        let unique = HashMap::from([(one_key.clone(), vec![one])]);
        assert_eq!(
            globally_unique_match_keys(&unique),
            HashSet::from([one_key])
        );

        let first = candidate("session-1", 42, "incarnation-1", "pane-1");
        let second = candidate("session-1", 43, "incarnation-2", "pane-2");
        let duplicate_session = HashMap::from([
            (first.target.key.clone(), vec![first]),
            (second.target.key.clone(), vec![second]),
        ]);
        assert!(globally_unique_match_keys(&duplicate_session).is_empty());

        let first = candidate("session-1", 42, "incarnation-1", "pane-1");
        let second = candidate("session-2", 42, "incarnation-1", "pane-2");
        let duplicate_process = HashMap::from([
            (first.target.key.clone(), vec![first]),
            (second.target.key.clone(), vec![second]),
        ]);
        assert!(globally_unique_match_keys(&duplicate_process).is_empty());

        let first = candidate("session-1", 42, "incarnation-1", "pane-1");
        let mut second = candidate("session-2", 43, "incarnation-2", "pane-1");
        second.agent.terminal_id = "terminal-pane-1-alias".to_string();
        let duplicate_pane = HashMap::from([
            (first.target.key.clone(), vec![first]),
            (second.target.key.clone(), vec![second]),
        ]);
        assert!(globally_unique_match_keys(&duplicate_pane).is_empty());

        let mut first = candidate("session-1", 42, "incarnation-1", "pane-1");
        let mut second = candidate("session-2", 43, "incarnation-2", "pane-2");
        first.agent.terminal_id = "terminal-shared".to_string();
        second.agent.terminal_id = "terminal-shared".to_string();
        second.socket_path.clone_from(&first.socket_path);
        let duplicate_terminal = HashMap::from([
            (first.target.key.clone(), vec![first]),
            (second.target.key.clone(), vec![second]),
        ]);
        assert!(globally_unique_match_keys(&duplicate_terminal).is_empty());

        let mut first = candidate("session-1", 42, "incarnation-1", "pane-1");
        let mut second = candidate("session-2", 43, "incarnation-2", "pane-1");
        first.socket_path = named_test_socket("server-1");
        second.socket_path = named_test_socket("server-2");
        let first_key = first.target.key.clone();
        let second_key = second.target.key.clone();
        let separate_socket_namespaces = HashMap::from([
            (first_key.clone(), vec![first]),
            (second_key.clone(), vec![second]),
        ]);
        assert_eq!(
            globally_unique_match_keys(&separate_socket_namespaces),
            HashSet::from([first_key, second_key])
        );
    }

    #[test]
    fn complete_agent_snapshot_requires_unique_terminal_and_pane_identities() {
        let mut first_agent = pane("working", "session-1");
        let mut second_agent = pane("working", "session-2");
        first_agent.pane_id = "pane-shared".to_string();
        second_agent.pane_id = "pane-shared".to_string();
        first_agent.terminal_id = "terminal-1".to_string();
        second_agent.terminal_id = "terminal-2".to_string();
        assert!(!agent_snapshot_identities_are_unique(&[
            first_agent,
            second_agent
        ]));

        let mut first_agent = pane("working", "session-1");
        let mut second_agent = pane("working", "session-2");
        first_agent.pane_id = "pane-1".to_string();
        second_agent.pane_id = "pane-2".to_string();
        first_agent.terminal_id = "terminal-shared".to_string();
        second_agent.terminal_id = "terminal-shared".to_string();
        assert!(!agent_snapshot_identities_are_unique(&[
            first_agent,
            second_agent
        ]));

        let first_agent = pane("working", "session-1");
        let mut second_agent = pane("working", "session-2");
        second_agent.pane_id = "pane-2".to_string();
        second_agent.terminal_id = "terminal-2".to_string();
        assert!(agent_snapshot_identities_are_unique(&[
            first_agent,
            second_agent
        ]));
    }

    #[test]
    fn pane_ownership_requires_the_requested_pane_and_exact_pid() {
        let info = herdr::ProcessInfo {
            pane_id: "pane-1".to_string(),
            foreground_processes: vec![crate::herdr::ProcessPid { pid: 42 }],
        };
        assert!(process_info_owns_pid(&info, "pane-1", 42));
        assert!(!process_info_owns_pid(&info, "pane-2", 42));
        assert!(!process_info_owns_pid(&info, "pane-1", 43));
    }

    #[test]
    fn socket_paths_are_bounded_absolute_paths() {
        assert!(valid_socket(TEST_SOCKET));
        assert!(!valid_socket("relative.sock"));
        assert!(!valid_socket(&format!("{TEST_SOCKET}\nother")));
        assert!(!valid_socket(&named_test_socket(
            &"x".repeat(MAX_SOCKET_BYTES)
        )));
    }
}
