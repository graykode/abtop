use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckStatus {
    Ok,
    Warn,
    Error,
    Info,
}

impl CheckStatus {
    fn label(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Error => "error",
            CheckStatus::Info => "info",
        }
    }
}

impl Serialize for CheckStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
}

#[derive(Debug)]
struct Check {
    status: CheckStatus,
    name: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct DoctorReport<'a> {
    version: &'static str,
    status: &'static str,
    summary: DoctorSummary,
    checks: &'a [Check],
}

#[derive(Serialize)]
struct DoctorSummary {
    checks: usize,
    warnings: usize,
    errors: usize,
}

impl Serialize for Check {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("Check", 3)?;
        state.serialize_field("status", &self.status)?;
        state.serialize_field("name", self.name)?;
        state.serialize_field("detail", &self.detail)?;
        state.end()
    }
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Ok,
            name,
            detail: detail.into(),
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Warn,
            name,
            detail: detail.into(),
        }
    }

    fn error(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Error,
            name,
            detail: detail.into(),
        }
    }

    fn info(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Info,
            name,
            detail: detail.into(),
        }
    }
}

pub fn run_doctor() -> i32 {
    let checks = collect_checks();
    println!("abtop doctor\n");
    for check in &checks {
        println!(
            "  [{:<4}] {:<22} {}",
            check.status.label(),
            check.name,
            check.detail
        );
    }
    let summary = summarize_checks(&checks);
    println!(
        "\nsummary: {} checks, {} warnings, {} errors",
        summary.checks, summary.warnings, summary.errors
    );
    exit_code_for_checks(&checks)
}

pub fn run_doctor_json() -> i32 {
    let checks = collect_checks();
    let summary = summarize_checks(&checks);
    let report = DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        status: if summary.errors > 0 { "error" } else { "ok" },
        summary,
        checks: &checks,
    };

    match serde_json::to_string_pretty(&report) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("failed to render doctor JSON: {err}");
            return 1;
        }
    }

    exit_code_for_checks(&checks)
}

fn collect_checks() -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(Check::ok(
        "version",
        format!("abtop {}", env!("CARGO_PKG_VERSION")),
    ));
    checks.push(check_claude_config());
    checks.push(check_statusline());
    checks.push(check_rate_limit_file());
    checks.push(check_process_scan());
    checks.push(check_port_scan());
    checks.push(check_termination_tool());
    checks.push(check_tmux_jump());
    checks.push(check_opencode_database());
    checks.extend(check_optional_tools());
    checks
}

fn summarize_checks(checks: &[Check]) -> DoctorSummary {
    DoctorSummary {
        checks: checks.len(),
        warnings: checks
            .iter()
            .filter(|check| check.status == CheckStatus::Warn)
            .count(),
        errors: checks
            .iter()
            .filter(|check| check.status == CheckStatus::Error)
            .count(),
    }
}

fn exit_code_for_checks(checks: &[Check]) -> i32 {
    if checks
        .iter()
        .any(|check| check.status == CheckStatus::Error)
    {
        1
    } else {
        0
    }
}

fn claude_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"))
}

fn check_claude_config() -> Check {
    let dir = claude_dir();
    if dir.exists() {
        Check::ok("claude config", dir.display().to_string())
    } else {
        Check::warn(
            "claude config",
            format!("{} does not exist yet", dir.display()),
        )
    }
}

fn check_statusline() -> Check {
    let settings = claude_dir().join("settings.json");
    let Ok(text) = std::fs::read_to_string(&settings) else {
        return Check::warn(
            "rate-limit hook",
            format!("{} not found; run abtop --setup", settings.display()),
        );
    };
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return Check::warn(
            "rate-limit hook",
            format!("{} contains invalid JSON", settings.display()),
        );
    };
    let command = json
        .get("statusLine")
        .and_then(Value::as_object)
        .and_then(|status_line| status_line.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if command.contains("abtop-statusline.") {
        Check::ok("rate-limit hook", "abtop statusLine command is configured")
    } else if command.is_empty() {
        Check::warn("rate-limit hook", "missing statusLine; run abtop --setup")
    } else {
        Check::warn(
            "rate-limit hook",
            "statusLine is owned by another command; abtop --setup will not overwrite it",
        )
    }
}

fn check_rate_limit_file() -> Check {
    let path = claude_dir().join("abtop-rate-limits.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Check::info(
            "claude quota data",
            "not written yet; it appears after a Claude response with the hook active",
        );
    };
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return Check::warn("claude quota data", "rate limit file is invalid JSON");
    };
    let updated_at = json.get("updated_at").and_then(Value::as_u64).unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(updated_at);
    if updated_at == 0 {
        Check::warn("claude quota data", "missing updated_at timestamp")
    } else if age > 600 {
        Check::warn(
            "claude quota data",
            format!("stale by {}; start a new Claude response", format_age(age)),
        )
    } else {
        Check::ok(
            "claude quota data",
            format!("fresh, updated {} ago", format_age(age)),
        )
    }
}

fn check_process_scan() -> Check {
    let processes = crate::collector::process::get_process_info();
    if processes.is_empty() {
        Check::error("process scan", "no processes found")
    } else {
        Check::ok(
            "process scan",
            format!(
                "{} processes visible via {}",
                processes.len(),
                process_scan_provider()
            ),
        )
    }
}

fn check_port_scan() -> Check {
    if cfg!(windows) && !command_exists("netstat") {
        return Check::error(
            "port scan",
            "netstat not found; Windows port discovery needs it",
        );
    }
    if cfg!(all(not(target_os = "linux"), not(windows))) && !command_exists("lsof") {
        return Check::warn("port scan", "lsof not found; port panel will be empty");
    }

    let ports = crate::collector::process::get_listening_ports();
    let count: usize = ports.values().map(Vec::len).sum();
    Check::ok(
        "port scan",
        format!(
            "{} listening TCP ports visible via {}",
            count,
            port_scan_provider()
        ),
    )
}

fn check_termination_tool() -> Check {
    let tool = if cfg!(windows) { "taskkill" } else { "kill" };
    if command_exists(tool) {
        Check::ok(
            "process control",
            format!("{tool} available for x/X actions"),
        )
    } else {
        Check::warn(
            "process control",
            format!("{tool} not found; x/X termination actions will fail"),
        )
    }
}

fn check_tmux_jump() -> Check {
    if std::env::var_os("TMUX").is_none() {
        return Check::info("tmux jump", "not inside tmux; Enter-to-jump is disabled");
    }
    if command_exists("tmux") {
        Check::ok("tmux jump", "inside tmux and tmux command is available")
    } else {
        Check::warn("tmux jump", "TMUX is set but tmux command was not found")
    }
}

fn check_opencode_database() -> Check {
    let path = opencode_db_path();
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return Check::info(
            "opencode db",
            format!(
                "{} not found; OpenCode sessions will be hidden",
                path.display()
            ),
        );
    };
    if metadata.file_type().is_symlink() {
        return Check::warn(
            "opencode db",
            "database path is a symlink; collector skips it",
        );
    }
    if !command_exists("sqlite3") {
        return Check::info(
            "opencode db",
            "sqlite3 not found; OpenCode sessions will be hidden",
        );
    }

    let db = path.to_string_lossy().into_owned();
    let output = Command::new("sqlite3")
        .args(["-readonly", "-json", &db, "SELECT 1 AS ok;"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            Check::ok("opencode db", format!("{} is readable", path.display()))
        }
        Ok(output) => Check::warn(
            "opencode db",
            format!("sqlite3 could not read database (exit {})", output.status),
        ),
        Err(err) => Check::warn("opencode db", format!("sqlite3 failed: {err}")),
    }
}

fn opencode_db_path() -> PathBuf {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));
    data_dir.join("opencode").join("opencode.db")
}

fn process_scan_provider() -> &'static str {
    if cfg!(target_os = "linux") {
        "/proc"
    } else if cfg!(windows) {
        "sysinfo"
    } else {
        "ps"
    }
}

fn port_scan_provider() -> &'static str {
    if cfg!(target_os = "linux") {
        "/proc/net"
    } else if cfg!(windows) {
        "netstat"
    } else {
        "lsof"
    }
}

fn check_optional_tools() -> Vec<Check> {
    [
        ("git", "project git stats"),
        ("claude", "Claude sessions and optional summaries"),
        ("codex", "Codex session discovery"),
        ("opencode", "OpenCode live process discovery"),
        ("sqlite3", "OpenCode database reads"),
    ]
    .into_iter()
    .map(|(tool, purpose)| {
        if command_exists(tool) {
            Check::ok(tool, purpose)
        } else {
            Check::info(tool, format!("not found; {}", purpose))
        }
    })
    .collect()
}

fn command_exists(command: &str) -> bool {
    if Path::new(command).is_absolute() {
        return Path::new(command).exists();
    }
    let output = if cfg!(windows) {
        Command::new("where").arg(command).output()
    } else {
        Command::new("sh")
            .args(["-c", &format!("command -v {}", shell_quote(command))])
            .output()
    };
    output
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_age_uses_compact_units() {
        assert_eq!(format_age(12), "12s");
        assert_eq!(format_age(120), "2m");
        assert_eq!(format_age(7200), "2h");
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("ab'top"), "'ab'\\''top'");
    }

    #[test]
    fn doctor_exits_zero_for_warnings_only() {
        let checks = vec![
            Check::ok("version", "abtop test"),
            Check::warn("claude quota data", "stale quota data"),
            Check::info("tmux", "not found"),
        ];

        assert_eq!(exit_code_for_checks(&checks), 0);
    }

    #[test]
    fn doctor_distinguishes_hard_errors() {
        let checks = vec![Check::error("process scan", "no processes found")];

        assert_eq!(exit_code_for_checks(&checks), 1);
    }

    #[test]
    fn doctor_summary_counts_statuses() {
        let checks = vec![
            Check::ok("version", "abtop test"),
            Check::warn("claude quota data", "stale quota data"),
            Check::error("process scan", "no processes found"),
            Check::info("tmux jump", "not inside tmux"),
        ];

        let summary = summarize_checks(&checks);

        assert_eq!(summary.checks, 4);
        assert_eq!(summary.warnings, 1);
        assert_eq!(summary.errors, 1);
    }

    #[test]
    fn doctor_report_serializes_stable_json_shape() {
        let checks = vec![Check::ok("version", "abtop test")];
        let summary = summarize_checks(&checks);
        let report = DoctorReport {
            version: "test",
            status: "ok",
            summary,
            checks: &checks,
        };

        let json: Value = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();

        assert_eq!(json["version"], "test");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["summary"]["checks"], 1);
        assert_eq!(json["checks"][0]["status"], "ok");
        assert_eq!(json["checks"][0]["name"], "version");
    }
}
