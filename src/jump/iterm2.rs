//! iTerm2 backend.
//!
//! Match the process's controlling tty against an iTerm2 session, then focus
//! its pane/window via AppleScript. The tty discriminates iTerm2 from other
//! terminals (a tmux-hosted process has the tmux pty, not an iTerm2 session
//! tty), so this returns `NotApplicable` for non-iTerm2 hosts.
//!
//! Note: the first AppleScript call triggers a one-time macOS Automation
//! permission prompt; until granted, `osascript` exits non-zero and the
//! attempt surfaces as `Failed`.

use super::{interpret_osascript, pid_tty, JumpAttempt, TerminalJumper};
use std::process::Command;

pub struct ITerm2Jumper;

impl TerminalJumper for ITerm2Jumper {
    fn name(&self) -> &'static str {
        "iterm2"
    }

    fn try_jump(&self, pid: u32) -> JumpAttempt {
        let Some(tty) = pid_tty(pid) else {
            return JumpAttempt::NotApplicable;
        };
        let script = format!(
            r#"if application "iTerm2" is running then
  tell application "iTerm2"
    repeat with w in windows
      repeat with t in tabs of w
        repeat with s in sessions of t
          if tty of s is "{tty}" then
            select s
            select t
            select w
            activate
            return "FOUND"
          end if
        end repeat
      end repeat
    end repeat
  end tell
end if
return "NOTFOUND""#
        );
        match Command::new("osascript").arg("-e").arg(&script).output() {
            Ok(o) if o.status.success() => interpret_osascript(&String::from_utf8_lossy(&o.stdout)),
            Ok(o) => JumpAttempt::Failed(format!(
                "osascript error: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => JumpAttempt::Failed(format!("osascript not runnable ({e})")),
        }
    }
}
