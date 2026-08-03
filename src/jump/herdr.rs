//! Herdr backend.
//!
//! Herdr injects its pane and socket identity into every managed pane process.
//! The inherited pane ID is the fast path, while `agent list` recovers the
//! current public ID after a cross-workspace pane move. Before focusing, every
//! candidate is checked against Herdr's foreground-process inventory so a
//! stale ID or PID collision cannot redirect the jump.

#[cfg(unix)]
use super::parse_env_var;
use super::{JumpAttempt, TerminalJumper};
use crate::herdr::{
    error_code, format_command_failure, parse_focus_agent_list, parse_focused_agent,
    parse_focused_pane, parse_process_info, CommandOutput, FocusAgentPane, HERDR_ENV,
    HERDR_PANE_ID, HERDR_SOCKET_PATH,
};
use std::collections::HashSet;
#[cfg(unix)]
use std::ffi::OsStr;
use std::time::{Duration, Instant};

const MAX_ID_BYTES: usize = 512;
const MAX_PID_AGENT_ROWS: usize = 256;
const MAX_PID_PANE_PROBES: usize = 32;
const PID_JUMP_TIMEOUT: Duration = Duration::from_secs(1);
const PID_COMMAND_TIMEOUT: Duration = Duration::from_millis(500);
const PID_COMPLETION_RESERVE: Duration = Duration::from_millis(25);
const SEMANTIC_JUMP_TIMEOUT: Duration = Duration::from_secs(1);
const SEMANTIC_COMMAND_TIMEOUT: Duration = Duration::from_millis(500);

pub struct HerdrJumper;

#[derive(Default)]
struct TargetHerdrEnv {
    socket_path: Option<String>,
    pane_id: Option<String>,
}

impl TargetHerdrEnv {
    fn get(&self, name: &str) -> Option<String> {
        match name {
            HERDR_SOCKET_PATH => self.socket_path.clone(),
            HERDR_PANE_ID => self.pane_id.clone(),
            _ => None,
        }
    }
}

impl TerminalJumper for HerdrJumper {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn try_jump(&self, pid: u32) -> JumpAttempt {
        try_pid_jump_with_budget(
            pid,
            PID_JUMP_TIMEOUT,
            PID_COMMAND_TIMEOUT,
            |name| std::env::var(name).ok(),
            |deadline| read_target_herdr_env_before_deadline(pid, deadline),
            crate::herdr::run_herdr_until,
        )
    }
}

fn try_pid_jump_with_budget(
    pid: u32,
    total_timeout: Duration,
    command_timeout: Duration,
    mut current_env: impl FnMut(&str) -> Option<String>,
    read_target_env: impl FnOnce(Instant) -> Result<TargetHerdrEnv, String>,
    mut run: impl FnMut(&str, &[String], Instant) -> Result<CommandOutput, String>,
) -> JumpAttempt {
    let completion_deadline = Instant::now() + total_timeout;
    let work_deadline = completion_deadline
        .checked_sub(PID_COMPLETION_RESERVE)
        .unwrap_or(completion_deadline);
    let current_herdr = non_empty(current_env(HERDR_ENV));
    if current_herdr.as_deref() != Some("1") {
        return JumpAttempt::NotApplicable;
    }
    let Some(current_socket) = non_empty(current_env(HERDR_SOCKET_PATH)) else {
        return JumpAttempt::NotApplicable;
    };
    let target_env = match read_target_env(work_deadline) {
        Ok(target_env) => target_env,
        Err(message) => return JumpAttempt::Failed(message),
    };

    try_jump_with(
        pid,
        |name| match name {
            HERDR_ENV => current_herdr.clone(),
            HERDR_SOCKET_PATH => Some(current_socket.clone()),
            _ => None,
        },
        |name| target_env.get(name),
        |socket_path, args| {
            let now = Instant::now();
            if now >= work_deadline {
                return Err("CLI timed out".to_string());
            }
            let command_deadline = now
                .checked_add(command_timeout)
                .unwrap_or(work_deadline)
                .min(work_deadline);
            run(socket_path, args, command_deadline)
        },
    )
}

fn remaining_before(deadline: Instant) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    (!remaining.is_zero()).then_some(remaining)
}

#[cfg(unix)]
fn read_target_herdr_env_before_deadline(
    pid: u32,
    deadline: Instant,
) -> Result<TargetHerdrEnv, String> {
    let _remaining = remaining_before(deadline)
        .ok_or_else(|| "process environment lookup timed out".to_string())?;
    let args = strings(&["eww", "-p", &pid.to_string()]);
    let output = crate::herdr::run_bounded_command_until(OsStr::new("ps"), &args, deadline)
        .map_err(|error| format!("process environment lookup failed ({error})"))?;
    if !output.success {
        return Ok(TargetHerdrEnv::default());
    }
    let output = String::from_utf8_lossy(&output.stdout);
    Ok(TargetHerdrEnv {
        socket_path: parse_env_var(&output, HERDR_SOCKET_PATH),
        pane_id: parse_env_var(&output, HERDR_PANE_ID),
    })
}

#[cfg(not(unix))]
fn read_target_herdr_env_before_deadline(
    pid: u32,
    deadline: Instant,
) -> Result<TargetHerdrEnv, String> {
    let remaining = remaining_before(deadline)
        .ok_or_else(|| "process environment lookup timed out".to_string())?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(TargetHerdrEnv {
            socket_path: crate::collector::process::read_process_env_var(pid, HERDR_SOCKET_PATH),
            pane_id: crate::collector::process::read_process_env_var(pid, HERDR_PANE_ID),
        });
    });
    rx.recv_timeout(remaining)
        .map_err(|_| "process environment lookup timed out".to_string())
}

/// Focus an exact provider-native session reported by Herdr.
///
/// Unlike PID-based terminal actions, this path is safe for lifecycle rows
/// whose status or process-action ownership is unavailable: Herdr's native
/// session reference is itself the focus identity. It never authorizes a PID
/// action and falls through when Herdr has no exact session reference.
pub(super) fn try_session_jump(provider: &str, session_id: &str) -> JumpAttempt {
    let deadline = Instant::now() + SEMANTIC_JUMP_TIMEOUT;
    try_session_jump_with(
        provider,
        session_id,
        |name| std::env::var(name).ok(),
        |socket_path, args| {
            run_herdr_before_deadline(socket_path, args, deadline, SEMANTIC_COMMAND_TIMEOUT)
        },
    )
}

fn run_herdr_before_deadline(
    socket_path: &str,
    args: &[String],
    deadline: Instant,
    command_timeout: Duration,
) -> Result<CommandOutput, String> {
    let now = Instant::now();
    if now >= deadline {
        return Err("CLI timed out".to_string());
    }
    let command_deadline = now
        .checked_add(command_timeout)
        .unwrap_or(deadline)
        .min(deadline);
    crate::herdr::run_herdr_until(socket_path, args, command_deadline)
}

fn try_session_jump_with(
    provider: &str,
    session_id: &str,
    mut current_env: impl FnMut(&str) -> Option<String>,
    mut run: impl FnMut(&str, &[String]) -> Result<CommandOutput, String>,
) -> JumpAttempt {
    if non_empty(current_env(HERDR_ENV)).as_deref() != Some("1") {
        return JumpAttempt::NotApplicable;
    }
    let Some(socket_path) = non_empty(current_env(HERDR_SOCKET_PATH)) else {
        return JumpAttempt::NotApplicable;
    };
    if !valid_identity_component(provider) || !valid_identity_component(session_id) {
        return JumpAttempt::NotApplicable;
    }

    let first = match list_agents(&socket_path, &mut run) {
        Ok(agents) => agents,
        Err(message) => return JumpAttempt::Failed(message),
    };
    let first_pane = match unique_session_pane(&first, provider, session_id) {
        Ok(Some(pane_id)) => pane_id,
        Ok(None) => return JumpAttempt::NotApplicable,
        Err(message) => return JumpAttempt::Failed(message),
    };

    // Re-read immediately before focus. A single cross-workspace pane move is
    // accepted by following the new unique pane ID; disappearance, duplication,
    // or any identity drift fails closed.
    let second = match list_agents(&socket_path, &mut run) {
        Ok(agents) => agents,
        Err(message) => return JumpAttempt::Failed(message),
    };
    let pane_id = match unique_session_pane(&second, provider, session_id) {
        Ok(Some(pane_id)) => pane_id,
        Ok(None) => {
            return JumpAttempt::Failed(format!(
                "selected {provider} session disappeared before focus"
            ))
        }
        Err(message) => return JumpAttempt::Failed(message),
    };
    let _moved = pane_id != first_pane;
    focus_session_pane(&socket_path, &pane_id, provider, session_id, &mut run)
}

fn list_agents(
    socket_path: &str,
    run: &mut impl FnMut(&str, &[String]) -> Result<CommandOutput, String>,
) -> Result<Vec<FocusAgentPane>, String> {
    let args = strings(&["agent", "list"]);
    let output = run(socket_path, &args)?;
    if !output.success {
        return Err(format_command_failure("agent list", &output));
    }
    parse_focus_agent_list(&output.stdout)
}

fn unique_session_pane(
    agents: &[FocusAgentPane],
    provider: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    let mut matching = agents
        .iter()
        .filter(|agent| agent_matches_session_identity(agent, provider, session_id));
    let Some(agent) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err(format!(
            "multiple panes claim the selected {provider} session"
        ));
    }
    Ok(Some(agent.pane_id.clone()))
}

fn agent_matches_session_identity(
    agent: &FocusAgentPane,
    provider: &str,
    session_id: &str,
) -> bool {
    let expected_source = format!("herdr:{provider}");
    valid_identity_component(&agent.pane_id)
        && agent.agent.as_deref() == Some(provider)
        && agent.agent_session.as_ref().is_some_and(|session| {
            valid_identity_component(&session.source)
                && valid_identity_component(&session.agent)
                && valid_identity_component(&session.kind)
                && valid_identity_component(&session.value)
                && session.source == expected_source
                && session.agent == provider
                && session.kind == "id"
                && session.value == session_id
        })
}

fn valid_identity_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && !value
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneProbe {
    OwnsPid,
    DifferentPid,
    Missing,
}

fn try_jump_with(
    pid: u32,
    mut current_env: impl FnMut(&str) -> Option<String>,
    mut target_env: impl FnMut(&str) -> Option<String>,
    mut run: impl FnMut(&str, &[String]) -> Result<CommandOutput, String>,
) -> JumpAttempt {
    if non_empty(current_env(HERDR_ENV)).as_deref() != Some("1") {
        return JumpAttempt::NotApplicable;
    }
    let Some(socket_path) = non_empty(current_env(HERDR_SOCKET_PATH)) else {
        return JumpAttempt::NotApplicable;
    };

    let target_socket = non_empty(target_env(HERDR_SOCKET_PATH));
    let same_session_claimed = target_socket.as_deref() == Some(socket_path.as_str());

    let inherited_pane = non_empty(target_env(HERDR_PANE_ID));
    let mut inspected = HashSet::new();
    let mut probes = 0_usize;
    if let Some(pane_id) = inherited_pane.as_deref() {
        inspected.insert(pane_id.to_string());
        probes += 1;
        match probe_pane(&socket_path, pane_id, pid, &mut run) {
            Ok(PaneProbe::OwnsPid) => {
                return focus_pane(&socket_path, pane_id, &mut run);
            }
            Ok(PaneProbe::DifferentPid | PaneProbe::Missing) => {}
            Err(message) => return JumpAttempt::Failed(message),
        }
    }

    let list_args = strings(&["agent", "list"]);
    let list_output = match run(&socket_path, &list_args) {
        Ok(output) => output,
        Err(message) => return JumpAttempt::Failed(message),
    };
    if !list_output.success {
        return JumpAttempt::Failed(format_command_failure("agent list", &list_output));
    }
    let listed = match parse_focus_agent_list(&list_output.stdout) {
        Ok(agents) => agents,
        Err(error) => {
            return JumpAttempt::Failed(error);
        }
    };
    if listed.len() > MAX_PID_AGENT_ROWS {
        return JumpAttempt::Failed(format!(
            "agent list returned more than {MAX_PID_AGENT_ROWS} rows"
        ));
    }

    let mut matches = Vec::new();
    for agent in listed {
        if agent.pane_id.is_empty() || !inspected.insert(agent.pane_id.clone()) {
            continue;
        }
        if probes >= MAX_PID_PANE_PROBES {
            return JumpAttempt::Failed(format!(
                "agent list required more than {MAX_PID_PANE_PROBES} pane probes"
            ));
        }
        probes += 1;
        match probe_pane(&socket_path, &agent.pane_id, pid, &mut run) {
            Ok(PaneProbe::OwnsPid) => matches.push(agent.pane_id),
            Ok(PaneProbe::DifferentPid | PaneProbe::Missing) => {}
            Err(message) => return JumpAttempt::Failed(message),
        }
    }

    match matches.as_slice() {
        [pane_id] => focus_pane(&socket_path, pane_id, &mut run),
        [] if same_session_claimed => {
            JumpAttempt::Failed(format!("no current pane owns PID {pid}"))
        }
        [] => JumpAttempt::NotApplicable,
        _ => JumpAttempt::Failed(format!("multiple panes claim PID {pid}")),
    }
}

fn probe_pane(
    socket_path: &str,
    pane_id: &str,
    pid: u32,
    run: &mut impl FnMut(&str, &[String]) -> Result<CommandOutput, String>,
) -> Result<PaneProbe, String> {
    let args = strings(&["pane", "process-info", "--pane", pane_id]);
    let output = run(socket_path, &args)?;
    if !output.success {
        if error_code(&output).as_deref() == Some("pane_not_found") {
            return Ok(PaneProbe::Missing);
        }
        return Err(format_command_failure("pane process-info", &output));
    }

    let info = parse_process_info(&output.stdout)?;
    if info.pane_id != pane_id {
        return Err(format!(
            "pane process-info returned {} for requested {pane_id}",
            info.pane_id
        ));
    }
    if info
        .foreground_processes
        .iter()
        .any(|process| process.pid == pid)
    {
        Ok(PaneProbe::OwnsPid)
    } else {
        Ok(PaneProbe::DifferentPid)
    }
}

fn focus_session_pane(
    socket_path: &str,
    pane_id: &str,
    provider: &str,
    session_id: &str,
    run: &mut impl FnMut(&str, &[String]) -> Result<CommandOutput, String>,
) -> JumpAttempt {
    let args = strings(&["agent", "focus", pane_id]);
    let output = match run(socket_path, &args) {
        Ok(output) => output,
        Err(message) => return JumpAttempt::Failed(message),
    };
    if !output.success {
        return JumpAttempt::Failed(format_command_failure("agent focus", &output));
    }
    let focused = match parse_focused_agent(&output.stdout) {
        Ok(agent) => agent,
        Err(error) => return JumpAttempt::Failed(error),
    };
    if focused.pane_id != pane_id {
        return JumpAttempt::Failed(format!(
            "agent focus returned {} for requested {pane_id}",
            focused.pane_id
        ));
    }
    if !agent_matches_session_identity(&focused, provider, session_id) {
        return JumpAttempt::Failed(format!(
            "agent focus did not confirm the selected {provider} session identity"
        ));
    }
    JumpAttempt::Jumped
}

fn focus_pane(
    socket_path: &str,
    pane_id: &str,
    run: &mut impl FnMut(&str, &[String]) -> Result<CommandOutput, String>,
) -> JumpAttempt {
    let args = strings(&["agent", "focus", pane_id]);
    let output = match run(socket_path, &args) {
        Ok(output) => output,
        Err(message) => return JumpAttempt::Failed(message),
    };
    if !output.success {
        return JumpAttempt::Failed(format_command_failure("agent focus", &output));
    }
    let focused_pane = match parse_focused_pane(&output.stdout) {
        Ok(pane_id) => pane_id,
        Err(error) => return JumpAttempt::Failed(error),
    };
    if focused_pane != pane_id {
        return JumpAttempt::Failed(format!(
            "agent focus returned {} for requested {pane_id}",
            focused_pane
        ));
    }
    JumpAttempt::Jumped
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|candidate| !candidate.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn ok(json: &str) -> Result<CommandOutput, String> {
        Ok(CommandOutput {
            success: true,
            status: "exit status: 0".to_string(),
            stdout: json.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    fn failed(json: &str) -> Result<CommandOutput, String> {
        Ok(CommandOutput {
            success: false,
            status: "exit status: 1".to_string(),
            stdout: Vec::new(),
            stderr: json.as_bytes().to_vec(),
        })
    }

    fn process_info(pane_id: &str, pids: &[u32]) -> String {
        let processes = pids
            .iter()
            .map(|pid| serde_json::json!({"pid": pid}))
            .collect::<Vec<_>>();
        serde_json::json!({
            "result": {
                "process_info": {
                    "pane_id": pane_id,
                    "foreground_processes": processes
                }
            }
        })
        .to_string()
    }

    fn agent_list(pane_ids: &[&str]) -> String {
        let agents = pane_ids
            .iter()
            .map(|pane_id| serde_json::json!({"pane_id": pane_id}))
            .collect::<Vec<_>>();
        serde_json::json!({"result": {"agents": agents}}).to_string()
    }

    fn session_agent_list(rows: &[(&str, &str, &str)]) -> String {
        let agents = rows
            .iter()
            .map(|(pane_id, provider, session_id)| {
                serde_json::json!({
                    "agent": provider,
                    "agent_session": {
                        "source": format!("herdr:{provider}"),
                        "agent": provider,
                        "kind": "id",
                        "value": session_id
                    },
                    "pane_id": pane_id,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({"result": {"agents": agents}}).to_string()
    }

    fn focused(pane_id: &str) -> String {
        serde_json::json!({"result": {"agent": {"pane_id": pane_id}}}).to_string()
    }

    fn focused_session(pane_id: &str, provider: &str, session_id: &str) -> String {
        serde_json::json!({
            "result": {
                "agent": {
                    "terminal_id": format!("terminal-{pane_id}"),
                    "agent": provider,
                    "agent_status": "idle",
                    "agent_session": {
                        "source": format!("herdr:{provider}"),
                        "agent": provider,
                        "kind": "id",
                        "value": session_id
                    },
                    "workspace_id": "workspace-1",
                    "tab_id": "tab-1",
                    "pane_id": pane_id,
                    "focused": true,
                    "state_change_seq": 9,
                    "revision": 11
                }
            }
        })
        .to_string()
    }

    fn scripted(
        script: Vec<(Vec<String>, Result<CommandOutput, String>)>,
    ) -> impl FnMut(&str, &[String]) -> Result<CommandOutput, String> {
        let mut script = VecDeque::from(script);
        move |socket_path, args| {
            assert_eq!(socket_path, "/tmp/herdr.sock");
            let (expected, output) = script.pop_front().expect("unexpected Herdr command");
            assert_eq!(args, expected.as_slice());
            output
        }
    }

    #[test]
    fn outside_herdr_is_not_applicable() {
        let mut called = false;
        let attempt = try_jump_with(
            42,
            |_| None,
            |_| None,
            |_, _| {
                called = true;
                ok("{}")
            },
        );
        assert_eq!(attempt, JumpAttempt::NotApplicable);
        assert!(!called);
    }

    #[test]
    fn expired_jump_deadline_fails_before_starting_the_cli() {
        let error = run_herdr_before_deadline(
            "/path/that/must/not/be-used.sock",
            &strings(&["agent", "list"]),
            Instant::now(),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error, "CLI timed out");
    }

    #[test]
    fn pid_jump_budget_includes_environment_lookup_and_command_elapsed() {
        let total_timeout = Duration::from_millis(300);
        let started = Instant::now();
        let mut environment_deadline = None;
        let mut command_deadline = None;
        let attempt = try_pid_jump_with_budget(
            42,
            total_timeout,
            Duration::from_millis(290),
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |deadline| {
                environment_deadline = Some(deadline);
                std::thread::sleep(Duration::from_millis(120));
                Ok(TargetHerdrEnv::default())
            },
            |_, _, deadline| {
                command_deadline = Some(deadline);
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                Err("CLI timed out".to_string())
            },
        );
        let elapsed = started.elapsed();

        assert_eq!(attempt, JumpAttempt::Failed("CLI timed out".to_string()));
        assert!(
            command_deadline.is_some() && command_deadline == environment_deadline,
            "the same aggregate absolute deadline did not reach both operations: env={environment_deadline:?}, command={command_deadline:?}"
        );
        assert!(
            elapsed < total_timeout + Duration::from_millis(40),
            "aggregate PID jump budget was exceeded: {elapsed:?}"
        );
    }

    #[test]
    fn exact_native_session_focus_ignores_lifecycle_status() {
        let list = session_agent_list(&[("w1:p2", "codex", "session-1")]);
        let script = vec![
            (strings(&["agent", "list"]), ok(&list)),
            (strings(&["agent", "list"]), ok(&list)),
            (
                strings(&["agent", "focus", "w1:p2"]),
                ok(&focused_session("w1:p2", "codex", "session-1")),
            ),
        ];
        let attempt = try_session_jump_with(
            "codex",
            "session-1",
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(attempt, JumpAttempt::Jumped);
    }

    #[test]
    fn exact_native_session_follows_one_pane_move() {
        let first = session_agent_list(&[("w1:p2", "codex", "session-1")]);
        let second = session_agent_list(&[("w2:p9", "codex", "session-1")]);
        let script = vec![
            (strings(&["agent", "list"]), ok(&first)),
            (strings(&["agent", "list"]), ok(&second)),
            (
                strings(&["agent", "focus", "w2:p9"]),
                ok(&focused_session("w2:p9", "codex", "session-1")),
            ),
        ];
        let attempt = try_session_jump_with(
            "codex",
            "session-1",
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(attempt, JumpAttempt::Jumped);
    }

    #[test]
    fn semantic_focus_rejects_a_mismatched_returned_session_identity() {
        let list = session_agent_list(&[("w1:p2", "codex", "session-1")]);
        let script = vec![
            (strings(&["agent", "list"]), ok(&list)),
            (strings(&["agent", "list"]), ok(&list)),
            (
                strings(&["agent", "focus", "w1:p2"]),
                ok(&focused_session("w1:p2", "codex", "session-2")),
            ),
        ];
        let attempt = try_session_jump_with(
            "codex",
            "session-1",
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(
            attempt,
            JumpAttempt::Failed(
                "agent focus did not confirm the selected codex session identity".to_string()
            )
        );
    }

    #[test]
    fn missing_or_mismatched_native_session_falls_through() {
        let list = session_agent_list(&[("w1:p2", "codex", "session-2")]);
        let script = vec![(strings(&["agent", "list"]), ok(&list))];
        let attempt = try_session_jump_with(
            "codex",
            "session-1",
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(attempt, JumpAttempt::NotApplicable);
    }

    #[test]
    fn duplicate_native_session_claims_fail_closed() {
        let list = session_agent_list(&[
            ("w1:p2", "codex", "session-1"),
            ("w2:p9", "codex", "session-1"),
        ]);
        let script = vec![(strings(&["agent", "list"]), ok(&list))];
        let attempt = try_session_jump_with(
            "codex",
            "session-1",
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert!(matches!(
            attempt,
            JumpAttempt::Failed(message) if message.contains("multiple panes claim")
        ));
    }

    #[test]
    fn semantic_match_requires_exact_source_agent_and_kind() {
        let parsed = parse_focus_agent_list(
            session_agent_list(&[("w1:p2", "codex", "session-1")]).as_bytes(),
        )
        .unwrap();
        assert_eq!(
            unique_session_pane(&parsed, "codex", "session-1").unwrap(),
            Some("w1:p2".to_string())
        );

        for field in ["source", "agent", "kind"] {
            let mut changed = parsed.clone();
            let session = changed[0].agent_session.as_mut().unwrap();
            match field {
                "source" => session.source = "herdr:claude".to_string(),
                "agent" => session.agent = "claude".to_string(),
                "kind" => session.kind = "path".to_string(),
                _ => unreachable!(),
            }
            assert_eq!(
                unique_session_pane(&changed, "codex", "session-1").unwrap(),
                None,
                "{field} must match exactly"
            );
        }
    }

    #[test]
    fn another_herdr_session_falls_through_after_current_server_check() {
        let script = vec![
            (
                strings(&["pane", "process-info", "--pane", "w2:p1"]),
                failed(r#"{"error":{"code":"pane_not_found","message":"pane not found"}}"#),
            ),
            (strings(&["agent", "list"]), ok(&agent_list(&[]))),
        ];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/other.sock".to_string()),
                HERDR_PANE_ID => Some("w2:p1".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(attempt, JumpAttempt::NotApplicable);
    }

    #[test]
    fn inherited_pane_with_exact_pid_focuses_directly() {
        let script = vec![
            (
                strings(&["pane", "process-info", "--pane", "w1:p2"]),
                ok(&process_info("w1:p2", &[7, 42])),
            ),
            (strings(&["agent", "focus", "w1:p2"]), ok(&focused("w1:p2"))),
        ];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                HERDR_PANE_ID => Some("w1:p2".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(attempt, JumpAttempt::Jumped);
    }

    #[test]
    fn stale_inherited_pane_falls_back_to_current_agent_list() {
        let script = vec![
            (
                strings(&["pane", "process-info", "--pane", "w1:p2"]),
                failed(r#"{"error":{"code":"pane_not_found","message":"pane not found"}}"#),
            ),
            (strings(&["agent", "list"]), ok(&agent_list(&["w2:p9"]))),
            (
                strings(&["pane", "process-info", "--pane", "w2:p9"]),
                ok(&process_info("w2:p9", &[42])),
            ),
            (strings(&["agent", "focus", "w2:p9"]), ok(&focused("w2:p9"))),
        ];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |name| match name {
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                HERDR_PANE_ID => Some("w1:p2".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(attempt, JumpAttempt::Jumped);
    }

    #[test]
    fn no_matching_current_server_pane_falls_through() {
        let script = vec![
            (strings(&["agent", "list"]), ok(&agent_list(&["w1:p1"]))),
            (
                strings(&["pane", "process-info", "--pane", "w1:p1"]),
                ok(&process_info("w1:p1", &[7])),
            ),
        ];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |_| None,
            scripted(script),
        );
        assert_eq!(attempt, JumpAttempt::NotApplicable);
    }

    #[test]
    fn claimed_same_session_without_current_pane_is_a_failure() {
        let script = vec![(strings(&["agent", "list"]), ok(&agent_list(&[])))];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |name| match name {
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(
            attempt,
            JumpAttempt::Failed("no current pane owns PID 42".to_string())
        );
    }

    #[test]
    fn multiple_pid_matches_fail_closed() {
        let script = vec![
            (
                strings(&["agent", "list"]),
                ok(&agent_list(&["w1:p1", "w1:p2"])),
            ),
            (
                strings(&["pane", "process-info", "--pane", "w1:p1"]),
                ok(&process_info("w1:p1", &[42])),
            ),
            (
                strings(&["pane", "process-info", "--pane", "w1:p2"]),
                ok(&process_info("w1:p2", &[42])),
            ),
        ];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |_| None,
            scripted(script),
        );
        assert_eq!(
            attempt,
            JumpAttempt::Failed("multiple panes claim PID 42".to_string())
        );
    }

    #[test]
    fn malformed_agent_list_is_reported() {
        let script = vec![(strings(&["agent", "list"]), ok("not-json"))];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |_| None,
            scripted(script),
        );
        assert!(matches!(
            attempt,
            JumpAttempt::Failed(message) if message.starts_with("agent list returned invalid JSON")
        ));
    }

    #[test]
    fn legacy_pid_focus_rejects_an_oversized_agent_list_before_probing() {
        let pane_ids = (0..=MAX_PID_AGENT_ROWS)
            .map(|index| format!("w1:p{index}"))
            .collect::<Vec<_>>();
        let pane_refs = pane_ids.iter().map(String::as_str).collect::<Vec<_>>();
        let script = vec![(strings(&["agent", "list"]), ok(&agent_list(&pane_refs)))];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |_| None,
            scripted(script),
        );
        assert_eq!(
            attempt,
            JumpAttempt::Failed(format!(
                "agent list returned more than {MAX_PID_AGENT_ROWS} rows"
            ))
        );
    }

    #[test]
    fn legacy_pid_focus_caps_the_number_of_pane_probes() {
        let pane_ids = (0..=MAX_PID_PANE_PROBES)
            .map(|index| format!("w1:p{index}"))
            .collect::<Vec<_>>();
        let pane_refs = pane_ids.iter().map(String::as_str).collect::<Vec<_>>();
        let mut script = vec![(strings(&["agent", "list"]), ok(&agent_list(&pane_refs)))];
        script.extend(pane_ids.iter().take(MAX_PID_PANE_PROBES).map(|pane_id| {
            (
                strings(&["pane", "process-info", "--pane", pane_id]),
                ok(&process_info(pane_id, &[])),
            )
        }));
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |_| None,
            scripted(script),
        );
        assert_eq!(
            attempt,
            JumpAttempt::Failed(format!(
                "agent list required more than {MAX_PID_PANE_PROBES} pane probes"
            ))
        );
    }

    #[test]
    fn focus_response_must_name_the_requested_pane() {
        let script = vec![
            (
                strings(&["pane", "process-info", "--pane", "w1:p2"]),
                ok(&process_info("w1:p2", &[42])),
            ),
            (strings(&["agent", "focus", "w1:p2"]), ok(&focused("w1:p3"))),
        ];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |name| match name {
                HERDR_PANE_ID => Some("w1:p2".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(
            attempt,
            JumpAttempt::Failed("agent focus returned w1:p3 for requested w1:p2".to_string())
        );
    }

    #[test]
    fn command_failure_uses_structured_error_message() {
        let script = vec![
            (
                strings(&["pane", "process-info", "--pane", "w1:p2"]),
                ok(&process_info("w1:p2", &[42])),
            ),
            (
                strings(&["agent", "focus", "w1:p2"]),
                failed(r#"{"error":{"code":"agent_not_found","message":"agent not found"}}"#),
            ),
        ];
        let attempt = try_jump_with(
            42,
            |name| match name {
                HERDR_ENV => Some("1".to_string()),
                HERDR_SOCKET_PATH => Some("/tmp/herdr.sock".to_string()),
                _ => None,
            },
            |name| match name {
                HERDR_PANE_ID => Some("w1:p2".to_string()),
                _ => None,
            },
            scripted(script),
        );
        assert_eq!(
            attempt,
            JumpAttempt::Failed("agent focus exited exit status: 1: agent not found".to_string())
        );
    }
}
