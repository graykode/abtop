//! Herdr backend.
//!
//! Herdr injects its pane and socket identity into every managed pane process.
//! The inherited pane ID is the fast path, while `agent list` recovers the
//! current public ID after a cross-workspace pane move. Before focusing, every
//! candidate is checked against Herdr's foreground-process inventory so a
//! stale ID or PID collision cannot redirect the jump.

use super::{pid_env_var, JumpAttempt, TerminalJumper};
use serde::Deserialize;
use std::collections::HashSet;
use std::process::Command;

const HERDR_ENV: &str = "HERDR_ENV";
const HERDR_PANE_ID: &str = "HERDR_PANE_ID";
const HERDR_SOCKET_PATH: &str = "HERDR_SOCKET_PATH";

pub struct HerdrJumper;

impl TerminalJumper for HerdrJumper {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn try_jump(&self, pid: u32) -> JumpAttempt {
        try_jump_with(
            pid,
            |name| std::env::var(name).ok(),
            |name| pid_env_var(pid, name),
            run_herdr,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct AgentListEnvelope {
    result: AgentListResult,
}

#[derive(Debug, Deserialize)]
struct AgentListResult {
    #[serde(default)]
    agents: Vec<AgentPane>,
}

#[derive(Debug, Deserialize)]
struct AgentPane {
    pane_id: String,
}

#[derive(Debug, Deserialize)]
struct ProcessInfoEnvelope {
    result: ProcessInfoResult,
}

#[derive(Debug, Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

#[derive(Debug, Deserialize)]
struct ProcessInfo {
    pane_id: String,
    #[serde(default)]
    foreground_processes: Vec<ProcessPid>,
}

#[derive(Debug, Deserialize)]
struct ProcessPid {
    pid: u32,
}

#[derive(Debug, Deserialize)]
struct FocusEnvelope {
    result: FocusResult,
}

#[derive(Debug, Deserialize)]
struct FocusResult {
    agent: FocusedAgent,
}

#[derive(Debug, Deserialize)]
struct FocusedAgent {
    pane_id: String,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
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
    if let Some(pane_id) = inherited_pane.as_deref() {
        inspected.insert(pane_id.to_string());
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
    let listed = match serde_json::from_slice::<AgentListEnvelope>(&list_output.stdout) {
        Ok(envelope) => envelope,
        Err(error) => {
            return JumpAttempt::Failed(format!("agent list returned invalid JSON ({error})"));
        }
    };

    let mut matches = Vec::new();
    for agent in listed.result.agents {
        if agent.pane_id.is_empty() || !inspected.insert(agent.pane_id.clone()) {
            continue;
        }
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

    let envelope = serde_json::from_slice::<ProcessInfoEnvelope>(&output.stdout)
        .map_err(|error| format!("pane process-info returned invalid JSON ({error})"))?;
    let info = envelope.result.process_info;
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
    let envelope = match serde_json::from_slice::<FocusEnvelope>(&output.stdout) {
        Ok(envelope) => envelope,
        Err(error) => {
            return JumpAttempt::Failed(format!("agent focus returned invalid JSON ({error})"));
        }
    };
    if envelope.result.agent.pane_id != pane_id {
        return JumpAttempt::Failed(format!(
            "agent focus returned {} for requested {pane_id}",
            envelope.result.agent.pane_id
        ));
    }
    JumpAttempt::Jumped
}

fn run_herdr(socket_path: &str, args: &[String]) -> Result<CommandOutput, String> {
    let output = Command::new("herdr")
        .args(args)
        .env(HERDR_SOCKET_PATH, socket_path)
        .output()
        .map_err(|error| format!("CLI not runnable ({error})"))?;
    Ok(CommandOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|candidate| !candidate.trim().is_empty())
}

fn error_code(output: &CommandOutput) -> Option<String> {
    serde_json::from_slice::<ErrorEnvelope>(&output.stdout)
        .ok()
        .map(|envelope| envelope.error.code)
}

fn format_command_failure(action: &str, output: &CommandOutput) -> String {
    let detail = serde_json::from_slice::<ErrorEnvelope>(&output.stdout)
        .ok()
        .map(|envelope| envelope.error.message)
        .and_then(|message| concise_detail(message.as_bytes()))
        .or_else(|| concise_detail(&output.stderr))
        .or_else(|| concise_detail(&output.stdout));
    let mut message = format!("{action} exited {}", output.status);
    if let Some(detail) = detail {
        message.push_str(": ");
        message.push_str(&detail);
    }
    message
}

fn concise_detail(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut detail: String = normalized.chars().take(160).collect();
    if normalized.chars().count() > 160 {
        detail.push('…');
    }
    Some(detail)
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
            stdout: json.as_bytes().to_vec(),
            stderr: Vec::new(),
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

    fn focused(pane_id: &str) -> String {
        serde_json::json!({"result": {"agent": {"pane_id": pane_id}}}).to_string()
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
