//! Terminal "jump to session" backends.
//!
//! Each supported terminal multiplexer / emulator is a [`TerminalJumper`],
//! one per submodule. [`resolve`] walks an ordered registry (see [`jumpers`])
//! and the first adapter that recognizes the process wins — "first applicable
//! adapter wins".
//!
//! The three-way [`JumpAttempt`] is the crux that makes adapters composable:
//! `NotApplicable` means "not my terminal, try the next one", while `Failed`
//! is reserved for a real command error in the adapter that *did* own the
//! process. Only `Failed`/`Jumped` stop the walk.

mod cmux;
#[cfg(target_os = "macos")]
mod iterm2;
mod tmux;

use crate::app::JumpOutcome;
use std::collections::HashMap;
use std::process::Command;

/// Result of a single adapter's attempt to jump to a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpAttempt {
    /// This adapter's terminal does not host the process — try the next one.
    NotApplicable,
    /// Successfully focused the process's terminal/pane.
    Jumped,
    /// This adapter owns the process but the focus command errored.
    Failed(String),
}

/// A terminal backend that can focus the pane/tab/window running a given PID.
pub trait TerminalJumper {
    /// Short label, used to prefix failure messages in the status line.
    fn name(&self) -> &'static str;
    /// Attempt to focus the terminal hosting `pid`.
    fn try_jump(&self, pid: u32) -> JumpAttempt;
}

/// Walk the adapters in order; the first non-`NotApplicable` result decides.
/// All `NotApplicable` → `NoOp` (nothing happened, no error to report).
pub fn resolve(jumpers: &[Box<dyn TerminalJumper>], pid: u32) -> JumpOutcome {
    for j in jumpers {
        match j.try_jump(pid) {
            JumpAttempt::NotApplicable => continue,
            JumpAttempt::Jumped => return JumpOutcome::Jumped,
            JumpAttempt::Failed(msg) => {
                return JumpOutcome::Failed(format!("{}: {}", j.name(), msg))
            }
        }
    }
    JumpOutcome::NoOp
}

/// The registry: the single ordered source of truth for supported terminals.
/// Order = most specific first: cmux (env-tagged) → tmux (multiplexer) →
/// iTerm2 on macOS (emulator). They are mutually exclusive by tty, so order
/// only matters for the multiplexer-inside-emulator case.
#[cfg(target_os = "macos")]
pub fn jumpers() -> Vec<Box<dyn TerminalJumper>> {
    vec![
        Box::new(cmux::CmuxJumper),
        Box::new(tmux::TmuxJumper),
        Box::new(iterm2::ITerm2Jumper),
    ]
}

/// The registry: the single ordered source of truth for supported terminals.
/// Non-macOS builds exclude the iTerm2 adapter so standalone Linux/Windows
/// sessions no-op cleanly instead of trying macOS-only `osascript`.
#[cfg(not(target_os = "macos"))]
pub fn jumpers() -> Vec<Box<dyn TerminalJumper>> {
    vec![Box::new(cmux::CmuxJumper), Box::new(tmux::TmuxJumper)]
}

/// Entry point used by the app: run the selected PID through the registry.
pub fn run_jump(pid: u32) -> JumpOutcome {
    resolve(&jumpers(), pid)
}

// ---------------------------------------------------------------------------
// Shared parsing helpers (pure — unit-tested below).
// ---------------------------------------------------------------------------

/// Parse `ps -o tty= -p <pid>` output into a `/dev/...` path.
/// Returns `None` when the process has no controlling tty (`??` or empty).
fn parse_tty(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t == "??" {
        return None;
    }
    Some(format!("/dev/{t}"))
}

/// Extract `VAR=value` from a whitespace-separated `ps eww` environment dump.
fn parse_env_var(ps_output: &str, var: &str) -> Option<String> {
    let prefix = format!("{var}=");
    ps_output
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix(&prefix).map(|v| v.to_string()))
}

/// Map the osascript marker line to a jump attempt.
/// `FOUND` → jumped, anything else (incl. `NOTFOUND`/empty) → not applicable.
fn interpret_osascript(stdout: &str) -> JumpAttempt {
    if stdout.trim() == "FOUND" {
        JumpAttempt::Jumped
    } else {
        JumpAttempt::NotApplicable
    }
}

/// Find the `session:window.pane` target whose pane-PID owns the process,
/// given a `tmux list-panes -F '#{pane_pid} #{target}'` dump.
fn find_pane_target(list_output: &str, is_descendant: impl Fn(u32) -> bool) -> Option<String> {
    for line in list_output.lines() {
        let mut parts = line.splitn(2, ' ');
        let pane_pid: u32 = match parts.next().and_then(|p| p.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        let target = match parts.next() {
            Some(t) => t,
            None => continue,
        };
        if is_descendant(pane_pid) {
            return Some(target.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Shared I/O helpers (thin wrappers around `ps`; the parsing they delegate to
// is unit-tested above). Private to this module tree — adapter submodules
// reach them via `use super::…`.
// ---------------------------------------------------------------------------

/// Controlling tty of a process as a `/dev/...` path, via `ps -o tty=`.
fn pid_tty(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    parse_tty(&String::from_utf8_lossy(&out.stdout))
}

/// Read a single environment variable from a process via `ps eww`.
/// On macOS this returns the (same-user) process environment; values with
/// spaces are not supported, which is fine for the UUID/ID lookups here.
fn pid_env_var(pid: u32, var: &str) -> Option<String> {
    let out = Command::new("ps")
        .args(["eww", "-p", &pid.to_string()])
        .output()
        .ok()?;
    parse_env_var(&String::from_utf8_lossy(&out.stdout), var)
}

/// Walk the process tree (via `ps -eo pid,ppid`) to test whether `target`
/// descends from `ancestor`.
fn is_descendant_of(target: u32, ancestor: u32) -> bool {
    if target == ancestor {
        return true;
    }
    let output = match Command::new("ps").args(["-eo", "pid,ppid"]).output() {
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock(&'static str, JumpAttempt);
    impl TerminalJumper for Mock {
        fn name(&self) -> &'static str {
            self.0
        }
        fn try_jump(&self, _pid: u32) -> JumpAttempt {
            self.1.clone()
        }
    }

    fn boxed(m: Mock) -> Box<dyn TerminalJumper> {
        Box::new(m)
    }

    // ---- resolve / registry loop ----

    #[test]
    fn resolve_all_not_applicable_is_noop() {
        let js = vec![
            boxed(Mock("a", JumpAttempt::NotApplicable)),
            boxed(Mock("b", JumpAttempt::NotApplicable)),
        ];
        assert_eq!(resolve(&js, 123), JumpOutcome::NoOp);
    }

    #[test]
    fn resolve_first_jumped_wins() {
        let js = vec![
            boxed(Mock("a", JumpAttempt::Jumped)),
            boxed(Mock("b", JumpAttempt::Failed("should not reach".into()))),
        ];
        assert_eq!(resolve(&js, 123), JumpOutcome::Jumped);
    }

    #[test]
    fn resolve_skips_not_applicable_until_jump() {
        let js = vec![
            boxed(Mock("a", JumpAttempt::NotApplicable)),
            boxed(Mock("b", JumpAttempt::Jumped)),
        ];
        assert_eq!(resolve(&js, 123), JumpOutcome::Jumped);
    }

    #[test]
    fn resolve_failure_is_prefixed_with_adapter_name() {
        let js = vec![
            boxed(Mock("a", JumpAttempt::NotApplicable)),
            boxed(Mock(
                "iterm2",
                JumpAttempt::Failed("permission denied".into()),
            )),
        ];
        assert_eq!(
            resolve(&js, 123),
            JumpOutcome::Failed("iterm2: permission denied".to_string())
        );
    }

    // ---- parse_tty ----

    #[test]
    fn parse_tty_strips_and_prefixes_dev() {
        assert_eq!(parse_tty("ttys009\n").as_deref(), Some("/dev/ttys009"));
    }

    #[test]
    fn parse_tty_trims_surrounding_whitespace() {
        assert_eq!(parse_tty("  ttys010 ").as_deref(), Some("/dev/ttys010"));
    }

    #[test]
    fn parse_tty_no_controlling_tty_is_none() {
        assert_eq!(parse_tty("??"), None);
        assert_eq!(parse_tty("?? \n"), None);
        assert_eq!(parse_tty(""), None);
        assert_eq!(parse_tty("   "), None);
    }

    // ---- parse_env_var ----

    #[test]
    fn parse_env_var_finds_value() {
        let dump = "FOO=bar CMUX_WORKSPACE_ID=abc-123-DEF BAZ=1";
        assert_eq!(
            parse_env_var(dump, "CMUX_WORKSPACE_ID").as_deref(),
            Some("abc-123-DEF")
        );
    }

    #[test]
    fn parse_env_var_missing_is_none() {
        assert_eq!(parse_env_var("FOO=bar BAZ=1", "CMUX_WORKSPACE_ID"), None);
    }

    #[test]
    fn parse_env_var_does_not_match_prefix_substring() {
        // "CMUX_WORKSPACE_ID_X" must not satisfy a query for "CMUX_WORKSPACE_ID"
        assert_eq!(
            parse_env_var("CMUX_WORKSPACE_ID_X=nope", "CMUX_WORKSPACE_ID"),
            None
        );
    }

    // ---- interpret_osascript ----

    #[test]
    fn interpret_osascript_found_is_jumped() {
        assert_eq!(interpret_osascript("FOUND\n"), JumpAttempt::Jumped);
    }

    #[test]
    fn interpret_osascript_notfound_is_not_applicable() {
        assert_eq!(
            interpret_osascript("NOTFOUND\n"),
            JumpAttempt::NotApplicable
        );
        assert_eq!(interpret_osascript(""), JumpAttempt::NotApplicable);
    }

    #[test]
    fn jumpers_include_platform_backends() {
        #[cfg(target_os = "macos")]
        assert_eq!(jumpers().len(), 3);

        #[cfg(not(target_os = "macos"))]
        assert_eq!(jumpers().len(), 2);
    }

    // ---- find_pane_target (tmux) ----

    #[test]
    fn find_pane_target_returns_matching_pane() {
        let dump = "111 main:0.0\n222 work:1.2\n";
        let target = find_pane_target(dump, |pid| pid == 222);
        assert_eq!(target.as_deref(), Some("work:1.2"));
    }

    #[test]
    fn find_pane_target_none_when_no_pane_owns_pid() {
        let dump = "111 main:0.0\n222 work:1.2\n";
        assert_eq!(find_pane_target(dump, |_| false), None);
    }

    #[test]
    fn find_pane_target_skips_malformed_lines() {
        let dump = "garbage\n\n333 dev:2.0\n";
        let target = find_pane_target(dump, |pid| pid == 333);
        assert_eq!(target.as_deref(), Some("dev:2.0"));
    }
}
