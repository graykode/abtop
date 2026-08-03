use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const STATUSLINE_SCRIPT: &str = r#"#!/bin/bash
# abtop StatusLine hook — writes rate limit data for abtop to read.
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

const KIMI_USAGES_SCRIPT: &str = include_str!("kimi_usages.py");
const KIMI_USAGES_SCRIPT_VERSION: &str = "abtop-kimi-usages-v2";
const KIMI_USAGES_SCRIPT_MARKER: &str = "abtop-kimi-usages-v";
const KIMI_REFRESH_INTERVAL: Duration = Duration::from_secs(120);
const KIMI_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

static KIMI_LAST_SPAWN: Mutex<Option<Instant>> = Mutex::new(None);

fn claude_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
}

fn script_path() -> PathBuf {
    claude_dir().join("abtop-statusline.sh")
}

fn settings_path() -> PathBuf {
    claude_dir().join("settings.json")
}

fn resolve_kimi_home() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(dir) = std::env::var("KIMI_CODE_HOME") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            candidates.push(path);
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".kimi-code"));
        candidates.push(home.join(".kimi"));
    }

    candidates
        .iter()
        .find(|home| home.join("credentials").join("kimi-code.json").is_file())
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

fn kimi_usages_script_path(home: &Path) -> PathBuf {
    home.join("abtop-usages.sh")
}

fn kimi_rate_file_path(home: &Path) -> PathBuf {
    home.join("abtop-rate-limits.json")
}

fn installed_kimi_usages_script() -> Option<(PathBuf, PathBuf)> {
    let home = resolve_kimi_home()?;
    let script = kimi_usages_script_path(&home);
    let content = fs::read_to_string(&script).ok()?;
    content
        .contains(KIMI_USAGES_SCRIPT_MARKER)
        .then_some((home, script))
}

fn install_kimi_usages_script(home: &Path) -> Result<PathBuf, std::io::Error> {
    fs::create_dir_all(home)?;
    let script = kimi_usages_script_path(home);
    let current = fs::read_to_string(&script).unwrap_or_default();
    if !current.contains(KIMI_USAGES_SCRIPT_VERSION) {
        fs::write(&script, KIMI_USAGES_SCRIPT)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    }
    Ok(script)
}

#[cfg(not(windows))]
fn spawn_kimi_companion(script: &Path) -> Option<Child> {
    Command::new(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

#[cfg(windows)]
fn spawn_kimi_companion(script: &Path) -> Option<Child> {
    ["python3", "python"].into_iter().find_map(|python| {
        Command::new(python)
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    })
}

fn run_kimi_companion(script: &Path, timeout: Duration) -> bool {
    let Some(mut child) = spawn_kimi_companion(script) else {
        return false;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn spawn_kimi_companion_background(script: &Path) -> bool {
    let Some(mut child) = spawn_kimi_companion(script) else {
        return false;
    };
    thread::spawn(move || {
        let _ = child.wait();
    });
    true
}

fn file_age(path: &Path) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Refresh the explicitly installed Kimi quota companion without blocking the
/// TUI. This never installs files and is not called by headless snapshot paths.
pub fn maybe_refresh_kimi_quota() {
    let Some((home, script)) = installed_kimi_usages_script() else {
        return;
    };
    if file_age(&kimi_rate_file_path(&home)).is_some_and(|age| age < KIMI_REFRESH_INTERVAL) {
        return;
    }

    let Ok(mut last_spawn) = KIMI_LAST_SPAWN.lock() else {
        return;
    };
    if last_spawn.is_some_and(|last| last.elapsed() < KIMI_REFRESH_INTERVAL) {
        return;
    }
    if spawn_kimi_companion_background(&script) {
        *last_spawn = Some(Instant::now());
    }
}

fn setup_claude_statusline() -> bool {
    println!("[claude] StatusLine hook");

    // Ensure ~/.claude directory exists
    let dir = claude_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("  ✗ failed to create {}: {}", dir.display(), e);
        return false;
    }

    // Step 1: Write the statusline script
    let script = script_path();
    match fs::write(&script, STATUSLINE_SCRIPT) {
        Ok(_) => {
            // chmod +x
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
            }
            println!("  ✓ wrote {}", script.display());
        }
        Err(e) => {
            eprintln!("  ✗ failed to write {}: {}", script.display(), e);
            return false;
        }
    }

    // Step 2: Update settings.json
    let settings_file = settings_path();
    let mut settings: Value = if settings_file.exists() {
        let content = match fs::read_to_string(&settings_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ✗ cannot read {}: {}", settings_file.display(), e);
                return false;
            }
        };
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "  ✗ {} contains invalid JSON: {}",
                    settings_file.display(),
                    e
                );
                eprintln!("    fix the file manually before running --setup");
                return false;
            }
        }
    } else {
        Value::Object(Default::default())
    };

    let Some(obj) = settings.as_object_mut() else {
        eprintln!("  ✗ {} must contain a JSON object", settings_file.display());
        return false;
    };

    // Check if statusLine is already configured
    let expected_cmd = script.display().to_string();
    if let Some(existing) = obj.get("statusLine") {
        if let Some(existing_obj) = existing.as_object() {
            if let Some(cmd) = existing_obj.get("command") {
                let cmd_str = cmd.as_str().unwrap_or("");
                if cmd_str != expected_cmd && !cmd_str.is_empty() {
                    eprintln!("  ⚠ statusLine already configured: {}", cmd_str);
                    eprintln!("    to override, remove the existing statusLine key from:");
                    eprintln!("    {}", settings_file.display());
                    return false;
                }
            }
        }
    }

    // Set statusLine config
    obj.insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": script.display().to_string()
        }),
    );

    match fs::write(
        &settings_file,
        serde_json::to_string_pretty(&settings).unwrap_or_default(),
    ) {
        Ok(_) => println!("  ✓ updated {}", settings_file.display()),
        Err(e) => {
            eprintln!("  ✗ failed to update {}: {}", settings_file.display(), e);
            return false;
        }
    }

    true
}

fn setup_kimi_usages() -> bool {
    println!("\n[kimi] quota companion");
    let Some(home) = resolve_kimi_home() else {
        eprintln!("  ✗ could not resolve the Kimi data directory");
        return false;
    };
    let script = match install_kimi_usages_script(&home) {
        Ok(script) => script,
        Err(error) => {
            eprintln!("  ✗ failed to install {}: {error}", home.display());
            return false;
        }
    };
    println!("  ✓ wrote {}", script.display());

    if run_kimi_companion(&script, KIMI_SETUP_TIMEOUT) {
        println!("  ✓ fetched initial quota data");
    } else {
        println!("  ⚠ quota fetch unavailable; run `kimi login` and restart abtop");
    }
    true
}

pub fn run_setup() {
    println!("abtop --setup: configuring local quota companions\n");

    let claude_ok = setup_claude_statusline();
    let kimi_ok = setup_kimi_usages();
    if !claude_ok && !kimi_ok {
        std::process::exit(1);
    }

    println!("\n  done!");
    println!("  Claude quota appears after the next Claude response.");
    println!("  Kimi quota refreshes while the TUI runs after explicit setup.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn file_age_distinguishes_missing_and_fresh_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("quota.json");
        assert_eq!(file_age(&path), None);

        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{{}}").unwrap();
        assert!(file_age(&path).is_some_and(|age| age < Duration::from_secs(2)));
    }

    #[cfg(unix)]
    #[test]
    fn timed_companion_is_killed_and_reaped() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("slow.sh");
        fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        assert!(!run_kimi_companion(&script, Duration::from_millis(100)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
