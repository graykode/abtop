use serde_json::Value;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const STATUSLINE_SCRIPT_SH: &str = r#"#!/bin/bash
# abtop StatusLine hook - writes rate limit data for abtop to read.
# Installed by: abtop --setup
# Reads JSON from stdin with a 5s timeout, pipes it to python via stdin
# to avoid ARG_MAX limits on large payloads.
INPUT=""
while IFS= read -r -t 5 line || [ -n "$line" ]; do
    INPUT="${INPUT}${line}
"
done
[ -z "$INPUT" ] && exit 0
printf '%s' "$INPUT" | python3 -c "
import sys, json, time, os
data = json.load(sys.stdin)
rl = data.get('rate_limits')
if not rl:
    sys.exit(0)
out = {'source': 'claude', 'updated_at': int(time.time())}
fh = rl.get('five_hour')
if fh:
    out['five_hour'] = {'used_percentage': fh.get('used_percentage', 0), 'resets_at': fh.get('resets_at', 0)}
sd = rl.get('seven_day')
if sd:
    out['seven_day'] = {'used_percentage': sd.get('used_percentage', 0), 'resets_at': sd.get('resets_at', 0)}
config_dir = os.environ.get('CLAUDE_CONFIG_DIR', os.path.join(os.path.expanduser('~'), '.claude'))
with open(os.path.join(config_dir, 'abtop-rate-limits.json'), 'w') as f:
    json.dump(out, f)
" 2>/dev/null
"#;

const STATUSLINE_SCRIPT_PS1: &str = r#"# abtop StatusLine hook - writes rate limit data for abtop to read.
# Installed by: abtop --setup
$ErrorActionPreference = "SilentlyContinue"

function Get-AbtopNumber($Value) {
    if ($null -eq $Value) {
        return 0
    }
    return $Value
}

$InputText = [Console]::In.ReadToEnd()
if ([string]::IsNullOrWhiteSpace($InputText)) {
    exit 0
}

$Data = $InputText | ConvertFrom-Json
$RateLimits = $Data.rate_limits
if ($null -eq $RateLimits) {
    exit 0
}

$Out = [ordered]@{
    source = "claude"
    updated_at = [int][DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
}

if ($null -ne $RateLimits.five_hour) {
    $Out.five_hour = @{
        used_percentage = Get-AbtopNumber $RateLimits.five_hour.used_percentage
        resets_at = Get-AbtopNumber $RateLimits.five_hour.resets_at
    }
}

if ($null -ne $RateLimits.seven_day) {
    $Out.seven_day = @{
        used_percentage = Get-AbtopNumber $RateLimits.seven_day.used_percentage
        resets_at = Get-AbtopNumber $RateLimits.seven_day.resets_at
    }
}

if ($env:CLAUDE_CONFIG_DIR) {
    $ConfigDir = $env:CLAUDE_CONFIG_DIR
} else {
    $ConfigDir = Join-Path $HOME ".claude"
}

New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
$OutputPath = Join-Path $ConfigDir "abtop-rate-limits.json"
$Json = $Out | ConvertTo-Json -Depth 4 -Compress
$Utf8NoBom = New-Object System.Text.UTF8Encoding $false
[System.IO.File]::WriteAllText($OutputPath, $Json, $Utf8NoBom)
"#;

#[derive(Debug)]
enum SetupError {
    Io {
        action: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    ExistingStatusLine {
        path: PathBuf,
        command: String,
    },
    SettingsRootNotObject {
        path: PathBuf,
    },
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetupError::Io {
                action,
                path,
                source,
            } => write!(f, "failed to {action} {}: {source}", path.display()),
            SetupError::InvalidJson { path, source } => {
                write!(f, "{} contains invalid JSON: {source}", path.display())
            }
            SetupError::ExistingStatusLine { path, command } => write!(
                f,
                "statusLine already configured as '{command}'. Remove the existing statusLine key from {} before running --setup again",
                path.display()
            ),
            SetupError::SettingsRootNotObject { path } => {
                write!(f, "{} must contain a JSON object", path.display())
            }
        }
    }
}

#[derive(Debug)]
struct SetupReport {
    script: PathBuf,
    settings: PathBuf,
}

fn claude_dir() -> PathBuf {
    claude_dir_from_env(std::env::var("CLAUDE_CONFIG_DIR").ok(), dirs::home_dir())
}

fn claude_dir_from_env(config_dir: Option<String>, home_dir: Option<PathBuf>) -> PathBuf {
    config_dir
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir.unwrap_or_default().join(".claude"))
}

fn script_name() -> &'static str {
    if cfg!(windows) {
        "abtop-statusline.ps1"
    } else {
        "abtop-statusline.sh"
    }
}

fn script_body() -> &'static str {
    if cfg!(windows) {
        STATUSLINE_SCRIPT_PS1
    } else {
        STATUSLINE_SCRIPT_SH
    }
}

fn script_path(dir: &Path) -> PathBuf {
    dir.join(script_name())
}

fn settings_path(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

fn statusline_command(script: &Path) -> String {
    if cfg!(windows) {
        format!(
            "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            script.display()
        )
    } else {
        script.display().to_string()
    }
}

pub fn run_setup() {
    crate::log_info!("setup start");
    println!("abtop --setup: configuring Claude Code StatusLine hook\n");

    match install_statusline(&claude_dir()) {
        Ok(report) => {
            crate::log_info!(
                "setup complete script={} settings={}",
                report.script.display(),
                report.settings.display()
            );
            println!("  wrote {}", report.script.display());
            println!("  updated {}", report.settings.display());
            println!(
                "\n  done! rate limit data will appear in abtop after the next Claude response."
            );
            println!("  restart any running Claude Code sessions to activate.");
        }
        Err(error) => {
            crate::log_error!("setup failed error={}", error);
            eprintln!("  error: {error}");
            if matches!(error, SetupError::InvalidJson { .. }) {
                eprintln!("  fix the file manually before running --setup");
            }
            std::process::exit(1);
        }
    }
}

fn install_statusline(dir: &Path) -> Result<SetupReport, SetupError> {
    fs::create_dir_all(dir).map_err(|source| SetupError::Io {
        action: "create",
        path: dir.to_path_buf(),
        source,
    })?;

    let script = script_path(dir);
    fs::write(&script, script_body()).map_err(|source| SetupError::Io {
        action: "write",
        path: script.clone(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).map_err(|source| {
            SetupError::Io {
                action: "chmod",
                path: script.clone(),
                source,
            }
        })?;
    }

    let settings = settings_path(dir);
    let mut settings_json = read_settings(&settings)?;
    let expected_cmd = statusline_command(&script);
    let obj = settings_json
        .as_object_mut()
        .ok_or_else(|| SetupError::SettingsRootNotObject {
            path: settings.clone(),
        })?;

    if let Some(existing_cmd) = obj
        .get("statusLine")
        .and_then(|existing| existing.as_object())
        .and_then(|existing| existing.get("command"))
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty() && !is_managed_statusline_command(command, dir))
    {
        return Err(SetupError::ExistingStatusLine {
            path: settings,
            command: existing_cmd.to_string(),
        });
    }

    obj.insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": expected_cmd
        }),
    );

    fs::write(
        &settings,
        serde_json::to_string_pretty(&settings_json).unwrap_or_default(),
    )
    .map_err(|source| SetupError::Io {
        action: "write",
        path: settings.clone(),
        source,
    })?;

    Ok(SetupReport { script, settings })
}

fn read_settings(settings: &Path) -> Result<Value, SetupError> {
    if !settings.exists() {
        return Ok(Value::Object(Default::default()));
    }

    let content = fs::read_to_string(settings).map_err(|source| SetupError::Io {
        action: "read",
        path: settings.to_path_buf(),
        source,
    })?;

    serde_json::from_str(&content).map_err(|source| SetupError::InvalidJson {
        path: settings.to_path_buf(),
        source,
    })
}

fn is_managed_statusline_command(command: &str, dir: &Path) -> bool {
    command == statusline_command(&script_path(dir))
        || command == dir.join("abtop-statusline.sh").display().to_string()
        || command == statusline_command(&dir.join("abtop-statusline.ps1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_installs_statusline_command_without_overwriting_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        fs::write(&settings, r#"{"theme":"dracula"}"#).unwrap();

        let report = install_statusline(dir.path()).unwrap();

        assert!(report.script.exists());
        let settings_json: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(settings_json["theme"], "dracula");
        assert_eq!(settings_json["statusLine"]["type"], "command");
        assert_eq!(
            settings_json["statusLine"]["command"],
            statusline_command(&report.script)
        );
    }

    #[test]
    fn claude_dir_uses_env_path_even_when_directory_does_not_exist() {
        let dir = PathBuf::from(r"C:\tmp\new-claude-config");

        assert_eq!(
            claude_dir_from_env(
                Some(dir.display().to_string()),
                Some(PathBuf::from(r"C:\Users\Example"))
            ),
            dir
        );
    }

    #[test]
    fn setup_refuses_to_overwrite_existing_statusline_command() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        fs::write(
            &settings,
            r#"{"statusLine":{"type":"command","command":"custom-hook"}}"#,
        )
        .unwrap();

        let error = install_statusline(dir.path()).unwrap_err();

        assert!(matches!(error, SetupError::ExistingStatusLine { .. }));
    }

    #[test]
    fn setup_upgrades_legacy_abtop_statusline_command() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        let legacy_command = dir.path().join("abtop-statusline.sh").display().to_string();
        fs::write(
            &settings,
            format!(
                r#"{{"statusLine":{{"type":"command","command":"{}"}}}}"#,
                legacy_command.replace('\\', "\\\\")
            ),
        )
        .unwrap();

        let report = install_statusline(dir.path()).unwrap();
        let settings_json: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();

        assert_eq!(
            settings_json["statusLine"]["command"],
            statusline_command(&report.script)
        );
    }

    #[test]
    fn setup_script_uses_native_platform_entrypoint() {
        let dir = tempfile::tempdir().unwrap();
        let report = install_statusline(dir.path()).unwrap();
        let script = fs::read_to_string(&report.script).unwrap();
        let settings_json: Value =
            serde_json::from_str(&fs::read_to_string(report.settings).unwrap()).unwrap();
        let command = settings_json["statusLine"]["command"].as_str().unwrap();

        if cfg!(windows) {
            assert!(report.script.ends_with("abtop-statusline.ps1"));
            assert!(script.contains("ConvertFrom-Json"));
            assert!(command.starts_with("powershell.exe -NoProfile"));
        } else {
            assert!(report.script.ends_with("abtop-statusline.sh"));
            assert!(script.starts_with("#!/bin/bash"));
            assert_eq!(command, report.script.display().to_string());
        }
    }
}
