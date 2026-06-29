//! tmux backend.
//!
//! Only applicable when abtop itself runs inside tmux (a `switch-client` needs
//! a tmux context). Maps the agent PID to the owning pane by process descent,
//! then switches client/window/pane. When the PID is in no tmux pane the
//! attempt is `NotApplicable`, letting another backend try.

use super::{find_pane_target, is_descendant_of, JumpAttempt, TerminalJumper};
use std::process::Command;

pub struct TmuxJumper;

impl TerminalJumper for TmuxJumper {
    fn name(&self) -> &'static str {
        "tmux"
    }

    fn try_jump(&self, pid: u32) -> JumpAttempt {
        if std::env::var("TMUX").is_err() {
            return JumpAttempt::NotApplicable;
        }
        let out = match Command::new("tmux")
            .args([
                "list-panes",
                "-a",
                "-F",
                "#{pane_pid} #{session_name}:#{window_index}.#{pane_index}",
            ])
            .output()
        {
            Ok(o) => o,
            Err(e) => return JumpAttempt::Failed(format!("tmux not runnable ({e})")),
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(target) = find_pane_target(&stdout, |pane_pid| is_descendant_of(pid, pane_pid))
        else {
            // PID not in any tmux pane — let another adapter try.
            return JumpAttempt::NotApplicable;
        };
        if let Some(session_name) = target.split(':').next() {
            let _ = Command::new("tmux")
                .args(["switch-client", "-t", session_name])
                .status();
        }
        if let Some(window) = target.split('.').next() {
            let _ = Command::new("tmux")
                .args(["select-window", "-t", window])
                .status();
        }
        let _ = Command::new("tmux")
            .args(["select-pane", "-t", &target])
            .status();
        JumpAttempt::Jumped
    }
}
