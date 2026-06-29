//! cmux (manaflow-ai) backend.
//!
//! Each cmux surface exports `CMUX_WORKSPACE_ID` (a UUID), inherited by the
//! agent process. We read it from the process environment and focus the
//! workspace via the cmux CLI, which accepts the UUID directly as `--workspace`.

use super::{pid_env_var, JumpAttempt, TerminalJumper};
use std::process::Command;

pub struct CmuxJumper;

impl TerminalJumper for CmuxJumper {
    fn name(&self) -> &'static str {
        "cmux"
    }

    fn try_jump(&self, pid: u32) -> JumpAttempt {
        let Some(workspace) = pid_env_var(pid, "CMUX_WORKSPACE_ID") else {
            return JumpAttempt::NotApplicable;
        };
        match Command::new("cmux")
            .args(["select-workspace", "--workspace", &workspace])
            .output()
        {
            Ok(o) if o.status.success() => JumpAttempt::Jumped,
            Ok(o) => JumpAttempt::Failed(format!("select-workspace exited {}", o.status)),
            Err(e) => JumpAttempt::Failed(format!("cmux CLI not runnable ({e})")),
        }
    }
}
