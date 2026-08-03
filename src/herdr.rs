//! Bounded client for the local Herdr CLI protocol.
//!
//! The monitor and terminal-jump adapter share these content-free response
//! types. Callers must still correlate a pane to an exact process before using
//! any returned state or issuing a focus request.

use serde::Deserialize;
use std::ffi::OsStr;
#[cfg(unix)]
use std::io;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) const HERDR_ENV: &str = "HERDR_ENV";
pub(crate) const HERDR_PANE_ID: &str = "HERDR_PANE_ID";
pub(crate) const HERDR_SOCKET_PATH: &str = "HERDR_SOCKET_PATH";
pub(crate) const MAX_COMMAND_OUTPUT: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    pub success: bool,
    pub status: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct StatusAgentListEnvelope {
    result: StatusAgentListResult,
}

#[derive(Debug, Deserialize)]
struct StatusAgentListResult {
    #[serde(default)]
    agents: Vec<StatusAgentPane>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct StatusAgentPane {
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_status: String,
    #[serde(default)]
    pub screen_detection_skipped: bool,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
    pub pane_id: String,
    pub state_change_seq: u64,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Deserialize)]
struct FocusAgentListEnvelope {
    result: FocusAgentListResult,
}

#[derive(Debug, Deserialize)]
struct FocusAgentListResult {
    #[serde(default)]
    agents: Vec<FocusAgentPane>,
}

/// Identity fields needed to focus a Herdr pane.
///
/// Lifecycle metadata deliberately does not enter this type, so a compatible
/// status-schema change cannot disable an otherwise exact focus operation.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct FocusAgentPane {
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionInfo>,
    pub pane_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
struct ProcessInfoEnvelope {
    result: ProcessInfoResult,
}

#[derive(Debug, Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub pane_id: String,
    #[serde(default)]
    pub foreground_processes: Vec<ProcessPid>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ProcessPid {
    pub pid: u32,
}

#[derive(Debug, Deserialize)]
struct FocusedPaneEnvelope {
    result: FocusedPaneResult,
}

#[derive(Debug, Deserialize)]
struct FocusedPaneResult {
    agent: FocusedPane,
}

#[derive(Debug, Deserialize)]
struct FocusedPane {
    pane_id: String,
}

#[derive(Debug, Deserialize)]
struct FocusedAgentEnvelope {
    result: FocusedAgentResult,
}

#[derive(Debug, Deserialize)]
struct FocusedAgentResult {
    agent: FocusAgentPane,
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

pub(crate) fn parse_status_agent_list(bytes: &[u8]) -> Result<Vec<StatusAgentPane>, String> {
    serde_json::from_slice::<StatusAgentListEnvelope>(bytes)
        .map(|envelope| envelope.result.agents)
        .map_err(|error| format!("agent list returned invalid JSON ({error})"))
}

pub(crate) fn parse_focus_agent_list(bytes: &[u8]) -> Result<Vec<FocusAgentPane>, String> {
    serde_json::from_slice::<FocusAgentListEnvelope>(bytes)
        .map(|envelope| envelope.result.agents)
        .map_err(|error| format!("agent list returned invalid JSON ({error})"))
}

pub(crate) fn parse_process_info(bytes: &[u8]) -> Result<ProcessInfo, String> {
    serde_json::from_slice::<ProcessInfoEnvelope>(bytes)
        .map(|envelope| envelope.result.process_info)
        .map_err(|error| format!("pane process-info returned invalid JSON ({error})"))
}

pub(crate) fn parse_focused_pane(bytes: &[u8]) -> Result<String, String> {
    serde_json::from_slice::<FocusedPaneEnvelope>(bytes)
        .map(|envelope| envelope.result.agent.pane_id)
        .map_err(|error| format!("agent focus returned invalid JSON ({error})"))
}

pub(crate) fn parse_focused_agent(bytes: &[u8]) -> Result<FocusAgentPane, String> {
    serde_json::from_slice::<FocusedAgentEnvelope>(bytes)
        .map(|envelope| envelope.result.agent)
        .map_err(|error| format!("agent focus returned invalid JSON ({error})"))
}

fn parse_error_body(bytes: &[u8]) -> Option<ErrorBody> {
    serde_json::from_slice::<ErrorEnvelope>(bytes)
        .ok()
        .map(|envelope| envelope.error)
}

pub(crate) fn error_code(output: &CommandOutput) -> Option<String> {
    parse_error_body(&output.stderr)
        .or_else(|| parse_error_body(&output.stdout))
        .map(|error| error.code)
}

pub(crate) fn format_command_failure(action: &str, output: &CommandOutput) -> String {
    let detail = parse_error_body(&output.stderr)
        .or_else(|| parse_error_body(&output.stdout))
        .map(|error| error.message)
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

/// Run one Herdr CLI request with null stdin, capped output, and a deadline.
pub(crate) fn run_herdr(
    socket_path: &str,
    args: &[String],
    timeout: Duration,
) -> Result<CommandOutput, String> {
    run_command(
        OsStr::new("herdr"),
        Some(socket_path),
        args,
        Instant::now() + timeout,
    )
}

/// Run one Herdr CLI request against an existing absolute deadline.
///
/// This form is used by multi-command actions so scheduling and setup time
/// between requests can never be added back to their aggregate budget.
pub(crate) fn run_herdr_until(
    socket_path: &str,
    args: &[String],
    deadline: Instant,
) -> Result<CommandOutput, String> {
    run_command(OsStr::new("herdr"), Some(socket_path), args, deadline)
}

/// Run one content-free local helper command with the same output and deadline
/// bounds as the Herdr client, but without injecting a Herdr socket context.
#[cfg(unix)]
pub(crate) fn run_bounded_command_until(
    binary: &OsStr,
    args: &[String],
    deadline: Instant,
) -> Result<CommandOutput, String> {
    run_command(binary, None, args, deadline)
}

fn run_command(
    binary: &OsStr,
    socket_path: Option<&str>,
    args: &[String],
    deadline: Instant,
) -> Result<CommandOutput, String> {
    if Instant::now() >= deadline {
        return Err("CLI timed out".to_string());
    }
    run_command_inner(binary, socket_path, args, deadline)
}

fn run_command_inner(
    binary: &OsStr,
    socket_path: Option<&str>,
    args: &[String],
    deadline: Instant,
) -> Result<CommandOutput, String> {
    if Instant::now() >= deadline {
        return Err("CLI timed out".to_string());
    }
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(socket_path) = socket_path {
        command.env(HERDR_SOCKET_PATH, socket_path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    // Rust's synchronous process creation API cannot be preempted safely. Run
    // it directly so a timed-out focus can never continue in a detached worker,
    // and bracket it with the same absolute deadline used by every controllable
    // wait, output drain, and cleanup phase.
    if Instant::now() >= deadline {
        return Err("CLI timed out".to_string());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("CLI not runnable ({error})"))?;
    if Instant::now() >= deadline {
        terminate_child_bounded(child, deadline);
        return Err("CLI timed out".to_string());
    }
    let Some(stdout) = child.stdout.take() else {
        terminate_child_bounded(child, deadline);
        return Err("CLI stdout pipe unavailable".to_string());
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child_bounded(child, deadline);
        return Err("CLI stderr pipe unavailable".to_string());
    };
    let (tx, rx) = mpsc::channel();
    spawn_bounded_reader(stdout, 0, tx.clone());
    spawn_bounded_reader(stderr, 1, tx);

    #[cfg(unix)]
    loop {
        match child_exited_without_reaping(child.id()) {
            Ok(true) => break,
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(5)),
                );
            }
            Ok(false) => {
                terminate_child_bounded(child, deadline);
                return Err("CLI timed out".to_string());
            }
            Err(error) => {
                terminate_child_bounded(child, deadline);
                return Err(format!("CLI wait failed ({error})"));
            }
        }
    }

    #[cfg(not(unix))]
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(5)),
                );
            }
            Ok(None) => {
                terminate_child_bounded(child, deadline);
                return Err("CLI timed out".to_string());
            }
            Err(error) => {
                terminate_child_bounded(child, deadline);
                return Err(format!("CLI wait failed ({error})"));
            }
        }
    };

    let now = Instant::now();
    if now >= deadline {
        terminate_child_bounded(child, deadline);
        return Err("CLI timed out".to_string());
    }
    let drain_deadline = output_drain_deadline(now, deadline);
    let mut streams: [Option<(Vec<u8>, bool)>; 2] = [None, None];
    while streams.iter().any(Option::is_none) {
        let now = Instant::now();
        if now >= drain_deadline {
            terminate_child_bounded(child, deadline);
            return Err("CLI retained an output pipe after exit".to_string());
        }
        match rx.recv_timeout(drain_deadline.saturating_duration_since(now)) {
            Ok((slot, bytes, truncated)) if slot < streams.len() => {
                streams[slot] = Some((bytes, truncated));
            }
            Ok(_) => {
                terminate_child_bounded(child, deadline);
                return Err("CLI returned an invalid output stream".to_string());
            }
            Err(_) => {
                terminate_child_bounded(child, deadline);
                return Err("CLI output drain timed out".to_string());
            }
        }
    }
    let (stdout, stdout_truncated) = streams[0].take().unwrap_or_default();
    let (stderr, stderr_truncated) = streams[1].take().unwrap_or_default();
    #[cfg(unix)]
    let status = finalize_exited_child(child, deadline)?;
    if stdout_truncated || stderr_truncated {
        return Err("CLI output exceeded 256 KiB".to_string());
    }
    Ok(command_output(status, stdout, stderr))
}

fn output_drain_deadline(now: Instant, command_deadline: Instant) -> Instant {
    command_deadline.min(now + Duration::from_millis(100))
}

fn command_output(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> CommandOutput {
    CommandOutput {
        success: status.success(),
        status: status.to_string(),
        stdout,
        stderr,
    }
}

fn spawn_bounded_reader(
    mut reader: impl Read + Send + 'static,
    slot: usize,
    tx: mpsc::Sender<(usize, Vec<u8>, bool)>,
) {
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut truncated = false;
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let remaining = MAX_COMMAND_OUTPUT.saturating_sub(output.len());
                    let retained = remaining.min(read);
                    output.extend_from_slice(&buffer[..retained]);
                    truncated |= retained < read;
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((slot, output, truncated));
    });
}

#[cfg(unix)]
fn child_exited_without_reaping(pid: u32) -> io::Result<bool> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { information.assume_init() };
    Ok(unsafe { information.si_pid() } != 0)
}

#[cfg(unix)]
fn finalize_exited_child(
    mut child: std::process::Child,
    deadline: Instant,
) -> Result<ExitStatus, String> {
    // The unreaped leader keeps its PID/PGID reserved while descendants are
    // terminated, so the group signal cannot hit an unrelated reused ID.
    signal_child_process_tree(&mut child);
    match child.try_wait() {
        Ok(Some(status)) => Ok(status),
        Ok(None) => {
            terminate_child_bounded(child, deadline);
            Err("CLI exit could not be reaped before its deadline".to_string())
        }
        Err(error) => {
            terminate_child_bounded(child, deadline);
            Err(format!("CLI wait failed ({error})"))
        }
    }
}

fn signal_child_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
}

fn terminate_child_bounded(mut child: std::process::Child, deadline: Instant) {
    #[cfg(not(unix))]
    {
        // Non-Unix polling may already have reaped the direct child. Never
        // signal its reusable numeric PID after that point.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
    }
    signal_child_process_tree(&mut child);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(1)),
                );
            }
            Ok(None) | Err(_) => break,
        }
    }

    // Never make the caller wait indefinitely for a broken platform reap.
    // The process was already terminated; a detached reaper prevents a zombie
    // if the kernel reports its exit after the command deadline.
    let _ = std::thread::Builder::new()
        .name("abtop-herdr-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::time::Instant;

    #[test]
    fn status_agent_list_parser_matches_herdr_0_7_5_omitted_false_shape() {
        let agents = parse_status_agent_list(
            br#"{"result":{"agents":[{"terminal_id":"t1","agent":"codex","agent_status":"blocked","agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"s1"},"workspace_id":"w1","tab_id":"tab1","pane_id":"p1","focused":false,"state_change_seq":9,"revision":11,"terminal_title":"secret prompt"}]}}"#,
        )
        .unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_status, "blocked");
        assert!(!agents[0].screen_detection_skipped);
        assert_eq!(agents[0].pane_id, "p1");
        assert_eq!(agents[0].state_change_seq, 9);
    }

    #[test]
    fn status_agent_list_parser_preserves_explicit_skipped_detection() {
        let agents = parse_status_agent_list(
            br#"{"result":{"agents":[{"terminal_id":"t1","agent":"codex","agent_status":"blocked","screen_detection_skipped":true,"agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"s1"},"pane_id":"p1","state_change_seq":9}]}}"#,
        )
        .unwrap();
        assert!(agents[0].screen_detection_skipped);
    }

    #[test]
    fn status_agent_list_parser_requires_sequence_metadata() {
        let missing_sequence = br#"{"result":{"agents":[{"terminal_id":"t1","agent":"codex","agent_status":"blocked","screen_detection_skipped":false,"agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"s1"},"pane_id":"p1"}]}}"#;
        assert!(parse_status_agent_list(missing_sequence).is_err());
    }

    #[test]
    fn focus_agent_list_parser_does_not_require_lifecycle_metadata() {
        let agents = parse_focus_agent_list(
            br#"{"result":{"agents":[{"agent":"codex","agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"s1"},"pane_id":"p1"}]}}"#,
        )
        .unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent.as_deref(), Some("codex"));
        assert_eq!(agents[0].pane_id, "p1");
    }

    #[test]
    fn focused_agent_parser_keeps_exact_session_identity() {
        let agent = parse_focused_agent(
            br#"{"result":{"agent":{"terminal_id":"t1","agent":"codex","agent_status":"idle","agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"s1"},"workspace_id":"w1","tab_id":"tab1","pane_id":"p1","focused":true,"state_change_seq":10,"revision":12}}}"#,
        )
        .unwrap();
        assert_eq!(agent.pane_id, "p1");
        assert_eq!(agent.agent.as_deref(), Some("codex"));
        assert_eq!(agent.agent_session.unwrap().value, "s1");
    }

    #[test]
    fn structured_command_errors_prefer_stderr_with_stdout_fallback() {
        let stderr_first = CommandOutput {
            success: false,
            status: "exit status: 1".to_string(),
            stdout: br#"{"error":{"code":"stdout_code","message":"stdout detail"}}"#.to_vec(),
            stderr: br#"{"error":{"code":"stderr_code","message":"stderr detail"}}"#.to_vec(),
        };
        assert_eq!(error_code(&stderr_first).as_deref(), Some("stderr_code"));
        assert_eq!(
            format_command_failure("agent focus", &stderr_first),
            "agent focus exited exit status: 1: stderr detail"
        );

        let stdout_compatibility = CommandOutput {
            stderr: Vec::new(),
            ..stderr_first
        };
        assert_eq!(
            error_code(&stdout_compatibility).as_deref(),
            Some("stdout_code")
        );
        assert_eq!(
            format_command_failure("agent focus", &stdout_compatibility),
            "agent focus exited exit status: 1: stdout detail"
        );
    }

    #[test]
    fn process_info_parser_keeps_only_pids() {
        let info = parse_process_info(
            br#"{"result":{"process_info":{"pane_id":"p1","foreground_processes":[{"pid":42,"argv":["secret"]}]}}}"#,
        )
        .unwrap();
        assert_eq!(info.pane_id, "p1");
        assert_eq!(info.foreground_processes[0].pid, 42);
    }

    #[test]
    fn bounded_reader_reports_truncated_output_without_growing_past_the_cap() {
        let (tx, rx) = mpsc::channel();
        spawn_bounded_reader(Cursor::new(vec![b'x'; MAX_COMMAND_OUTPUT + 1]), 0, tx);
        let (slot, output, truncated) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(slot, 0);
        assert_eq!(output.len(), MAX_COMMAND_OUTPUT);
        assert!(truncated);
    }

    #[test]
    fn output_drain_is_contained_by_the_command_deadline() {
        let now = Instant::now();
        let short_deadline = now + Duration::from_millis(25);
        assert_eq!(output_drain_deadline(now, short_deadline), short_deadline);
        assert_eq!(
            output_drain_deadline(now, now + Duration::from_secs(1)),
            now + Duration::from_millis(100)
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_kills_the_process_group_promptly() {
        let started = Instant::now();
        let error = run_command(
            OsStr::new("/bin/sh"),
            Some("/tmp/herdr-test.sock"),
            &["-c".to_string(), "sleep 5".to_string()],
            Instant::now() + Duration::from_millis(30),
        )
        .unwrap_err();
        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn retained_descendant_output_pipe_kills_the_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("descendant-survived");
        let started = Instant::now();
        let error = run_command(
            OsStr::new("/bin/sh"),
            Some(marker.to_str().unwrap()),
            &[
                "-c".to_string(),
                "(sleep 1; : > \"$HERDR_SOCKET_PATH\") &".to_string(),
            ],
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(
            error.contains("output pipe") || error.contains("output drain"),
            "unexpected error: {error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        std::thread::sleep(Duration::from_millis(1_100));
        assert!(
            !marker.exists(),
            "descendant survived after retaining the output pipe"
        );
    }
}
