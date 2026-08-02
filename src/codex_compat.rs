//! Temporary 0.6 compatibility for shell functions installed by pre-plugin builds.
//!
//! This is intentionally not an integration mechanism. It performs no relay,
//! status collection, argument filtering, or binary discovery. A legacy shell
//! function already loaded in the current shell may still call
//! `abtop codex -- ...`; in that case we replace this process with the exact
//! executable that the function captured in `ABTOP_MANAGED_CODEX_BINARY`.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

const LEGACY_CODEX_BINARY_ENV: &str = "ABTOP_MANAGED_CODEX_BINARY";

fn captured_binary() -> io::Result<PathBuf> {
    let binary = std::env::var_os(LEGACY_CODEX_BINARY_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "legacy Codex compatibility requires ABTOP_MANAGED_CODEX_BINARY",
        )
    })?;
    let path = PathBuf::from(binary);
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ABTOP_MANAGED_CODEX_BINARY must be an absolute path",
        ));
    }
    Ok(path)
}

/// Hand control directly to the native command captured by an already-loaded
/// legacy shell function. This entry point is scheduled for removal in 0.7.
pub(crate) fn run(args: Vec<OsString>) -> io::Result<i32> {
    let binary = captured_binary()?;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let error = std::process::Command::new(binary)
            .args(args)
            .env_remove(LEGACY_CODEX_BINARY_ENV)
            .exec();
        Err(error)
    }

    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(binary)
            .args(args)
            .env_remove(LEGACY_CODEX_BINARY_ENV)
            .status()?;
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_environment_name_stays_private_and_explicit() {
        assert_eq!(LEGACY_CODEX_BINARY_ENV, "ABTOP_MANAGED_CODEX_BINARY");
    }
}
