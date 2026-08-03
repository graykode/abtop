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

/// Companion script for Kimi Code account quota.
/// Network/auth stay outside abtop — this script writes the same local JSON
/// shape Claude's StatusLine hook produces (`abtop-rate-limits.json`).
///
/// Logic mirrors TokenTracker's `fetchKimiLimits` (OAuth refresh + usages API).
/// Marker so we can detect an outdated installed script and rewrite it.
const KIMI_USAGES_SCRIPT_VERSION: &str = "abtop-kimi-usages-v1";

/// Min gap between background companion spawns (matches TokenTracker ~2 min).
const KIMI_REFRESH_INTERVAL: Duration = Duration::from_secs(120);

/// Treat the local rate-limit file as fresh enough to skip a refresh spawn.
const KIMI_FILE_FRESH: Duration = Duration::from_secs(120);

/// File missing or older than this → cold start: run the companion once
/// synchronously so the first quota paint is not empty.
/// Matches the quota panel's "stale" dim threshold (10 minutes).
const KIMI_FILE_ANCIENT: Duration = Duration::from_secs(600);

/// Max wait for a cold-start companion run (network + token refresh).
const KIMI_COLD_START_TIMEOUT: Duration = Duration::from_secs(5);

static KIMI_LAST_SPAWN: Mutex<Option<Instant>> = Mutex::new(None);

const KIMI_USAGES_SCRIPT: &str = r#"#!/usr/bin/env python3
"""abtop Kimi Code usage companion — writes local rate-limit JSON for abtop.

# abtop-kimi-usages-v1

Installed automatically when abtop detects Kimi credentials, or via:
  abtop --setup

abtop may spawn this in the background; it only *reads* the JSON this writes.
Network/auth stay in this script — not in the abtop process.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

CLIENT_ID = "17e5f671-d194-4dfb-9706-5516cb48c098"
TOKEN_URL = "https://auth.kimi.com/api/oauth/token"
USAGES_URL = "https://api.kimi.com/coding/v1/usages"
TIMEOUT = 8


def kimi_home():
    explicit = os.environ.get("KIMI_CODE_HOME", "").strip()
    if explicit:
        return Path(explicit).expanduser()
    home = Path.home()
    code = home / ".kimi-code"
    creds = code / "credentials" / "kimi-code.json"
    if creds.is_file():
        try:
            data = json.loads(creds.read_text())
            if data.get("access_token"):
                return code
        except Exception:
            pass
    legacy = home / ".kimi"
    if (legacy / "credentials" / "kimi-code.json").is_file():
        return legacy
    return code


def load_creds(path):
    if not path.is_file():
        return None
    try:
        return json.loads(path.read_text())
    except Exception:
        return None


def save_creds(path, creds):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(creds, indent=2) + "\n")


def expired(creds, now=None):
    now = time.time() if now is None else now
    exp = creds.get("expires_at")
    try:
        exp_f = float(exp)
    except (TypeError, ValueError):
        return False
    if exp_f <= 0:
        return False
    return exp_f <= now + 30


def refresh_token(creds, creds_path):
    body = urllib.parse.urlencode(
        {
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": creds.get("refresh_token") or "",
        }
    ).encode()
    req = urllib.request.Request(
        TOKEN_URL,
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/x-www-form-urlencoded",
            "X-Msh-Platform": "kimi_cli",
        },
    )
    with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
        data = json.loads(resp.read().decode())
    if not data.get("access_token"):
        raise RuntimeError("token refresh missing access_token")
    expires_in = float(data.get("expires_in") or 900)
    if expires_in <= 0:
        expires_in = 900
    next_creds = {
        "access_token": str(data["access_token"]),
        "refresh_token": str(data.get("refresh_token") or creds.get("refresh_token") or ""),
        "expires_at": time.time() + expires_in,
        "scope": str(data.get("scope") or "kimi-code"),
        "token_type": str(data.get("token_type") or "Bearer"),
        "expires_in": expires_in,
    }
    save_creds(creds_path, next_creds)
    return next_creds


def fetch_usages(access_token):
    req = urllib.request.Request(
        USAGES_URL,
        headers={
            "Authorization": "Bearer " + access_token,
            "Accept": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
        return json.loads(resp.read().decode())


def num(value):
    if value is None or value == "":
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def parse_reset(value):
    if not isinstance(value, str) or not value:
        return None
    # fromisoformat handles trailing Z poorly on some Python versions; normalize.
    s = value.replace("Z", "+00:00")
    try:
        from datetime import datetime

        dt = datetime.fromisoformat(s)
        return int(dt.timestamp())
    except Exception:
        return None


def window_from_usage(data, window_minutes=None):
    if not isinstance(data, dict):
        return None
    limit = num(data.get("limit"))
    if limit is None or limit <= 0:
        return None
    used = num(data.get("used"))
    if used is None:
        remaining = num(data.get("remaining"))
        if remaining is None:
            return None
        used = limit - remaining
    pct = max(0.0, min(100.0, (used / limit) * 100.0))
    out = {
        "used_percentage": pct,
        "resets_at": parse_reset(
            data.get("resetTime") or data.get("reset_at") or data.get("resetAt")
        )
        or 0,
    }
    if window_minutes is not None:
        out["window_minutes"] = window_minutes
    return out


def window_minutes_from(entry):
    if not isinstance(entry, dict):
        return None
    window = entry.get("window")
    if not isinstance(window, dict):
        return None
    duration = num(window.get("duration"))
    if duration is None or duration <= 0:
        return None
    unit = str(window.get("timeUnit") or "TIME_UNIT_MINUTE")
    d = int(duration)
    if unit in ("TIME_UNIT_SECOND", "SECOND", "seconds"):
        return max(1, (d + 59) // 60)
    if unit in ("TIME_UNIT_HOUR", "HOUR", "hours"):
        return d * 60
    if unit in ("TIME_UNIT_DAY", "DAY", "days"):
        return d * 24 * 60
    return d


def normalize(body):
    limits = body.get("limits") if isinstance(body.get("limits"), list) else []
    short = None
    if limits:
        entry = limits[0] if isinstance(limits[0], dict) else None
        if entry is not None:
            detail = entry.get("detail") if isinstance(entry.get("detail"), dict) else entry
            mins = window_minutes_from(entry) or 300
            short = window_from_usage(detail, mins)
    long = window_from_usage(body.get("usage") if isinstance(body.get("usage"), dict) else None)
    if short is None and long is None:
        return None
    out = {"source": "kimi", "updated_at": int(time.time())}
    if short is not None:
        out["five_hour"] = short
    if long is not None:
        out["seven_day"] = long
    return out


def main():
    home = kimi_home()
    creds_path = home / "credentials" / "kimi-code.json"
    out_path = home / "abtop-rate-limits.json"
    creds = load_creds(creds_path)
    if not creds or not str(creds.get("access_token") or "").strip():
        print(f"abtop-usages: not logged in (missing {creds_path})", file=sys.stderr)
        print("  run: kimi login", file=sys.stderr)
        return 1
    try:
        if expired(creds) and creds.get("refresh_token"):
            creds = refresh_token(creds, creds_path)
        try:
            body = fetch_usages(str(creds["access_token"]))
        except urllib.error.HTTPError as e:
            if e.code == 401 and creds.get("refresh_token"):
                creds = refresh_token(creds, creds_path)
                body = fetch_usages(str(creds["access_token"]))
            else:
                raise
        out = normalize(body)
        if not out:
            print("abtop-usages: could not parse usages response", file=sys.stderr)
            return 1
        tmp = out_path.with_suffix(".tmp")
        tmp.write_text(json.dumps(out) + "\n")
        tmp.replace(out_path)
        print(f"abtop-usages: wrote {out_path}")
        return 0
    except Exception as e:
        print(f"abtop-usages: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
"#;

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

fn kimi_dir() -> PathBuf {
    resolve_kimi_home().unwrap_or_else(|| {
        std::env::var("KIMI_CODE_HOME")
            .ok()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".kimi-code"))
    })
}

fn kimi_usages_script_path_for(home: &Path) -> PathBuf {
    home.join("abtop-usages.sh")
}

fn kimi_rate_file_for(home: &Path) -> PathBuf {
    home.join("abtop-rate-limits.json")
}

fn kimi_creds_path_for(home: &Path) -> PathBuf {
    home.join("credentials").join("kimi-code.json")
}

/// Prefer a Kimi home that already has login credentials.
fn resolve_kimi_home() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(dir) = std::env::var("KIMI_CODE_HOME") {
        let p = PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            candidates.push(p);
        }
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".kimi-code"));
        candidates.push(home.join(".kimi"));
    }
    for dir in &candidates {
        if kimi_creds_path_for(dir).is_file() {
            return Some(dir.clone());
        }
    }
    candidates.into_iter().next()
}

fn kimi_has_credentials(home: &Path) -> bool {
    let path = kimi_creds_path_for(home);
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    // Cheap check — avoid parsing secrets into structured types here.
    text.contains("access_token") && text.contains('"')
}

/// Ensure the companion script exists (and is current), return its path.
pub fn ensure_kimi_usages_script() -> Option<PathBuf> {
    let home = resolve_kimi_home()?;
    fs::create_dir_all(&home).ok()?;
    let script = kimi_usages_script_path_for(&home);
    let needs_write = match fs::read_to_string(&script) {
        Ok(existing) => !existing.contains(KIMI_USAGES_SCRIPT_VERSION),
        Err(_) => true,
    };
    if needs_write {
        fs::write(&script, KIMI_USAGES_SCRIPT).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
        }
    }
    Some(script)
}

fn rate_file_age(path: &Path) -> Option<Duration> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn rate_file_is_fresh(path: &Path) -> bool {
    rate_file_age(path).is_some_and(|d| d < KIMI_FILE_FRESH)
}

/// Missing file, or older than [`KIMI_FILE_ANCIENT`] → worth a blocking cold start.
fn rate_file_is_cold(path: &Path) -> bool {
    match rate_file_age(path) {
        None => true,
        Some(age) => age >= KIMI_FILE_ANCIENT,
    }
}

/// Run the companion and wait up to `timeout`, then kill if still running.
fn run_companion_sync(script: &Path, timeout: Duration) {
    let mut child = match Command::new(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    wait_child_with_timeout(&mut child, timeout);
}

fn wait_child_with_timeout(child: &mut Child, timeout: Duration) {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return,
        }
    }
}

fn spawn_companion_background(script: &Path) {
    let _ = Command::new(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Out-of-the-box Kimi quota: if the user is logged in (`kimi login`), install
/// the companion script if needed and refresh when the local rate-limit file
/// is missing or stale.
///
/// - **Cold start** (file missing or ≥10 min old): run the companion once
///   synchronously (≤5s) so the first quota paint is populated.
/// - **Warm refresh** (file present but >2 min old): fire-and-forget background
///   spawn so the TUI never blocks.
///
/// abtop still does not perform HTTP itself — the child script does.
///
/// Call from the full TUI [`crate::app::App::tick`] path only (not
/// `tick_no_summaries`). Background path is non-blocking; cold start may block
/// briefly once.
pub fn maybe_refresh_kimi_quota() {
    let Some(home) = resolve_kimi_home() else {
        return;
    };
    if !kimi_has_credentials(&home) {
        return;
    }

    let rate_path = kimi_rate_file_for(&home);
    if rate_file_is_fresh(&rate_path) {
        return;
    }

    let cold = rate_file_is_cold(&rate_path);

    {
        let mut last = match KIMI_LAST_SPAWN.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(t) = *last {
            if t.elapsed() < KIMI_REFRESH_INTERVAL {
                return;
            }
        }
        *last = Some(Instant::now());
    }

    let Some(script) = ensure_kimi_usages_script() else {
        return;
    };

    if cold {
        run_companion_sync(&script, KIMI_COLD_START_TIMEOUT);
    } else {
        spawn_companion_background(&script);
    }
}

pub fn run_setup() {
    println!("abtop --setup: installing local rate-limit companions\n");

    setup_claude_statusline();
    setup_kimi_usages();

    println!("\n  done!");
    println!("  Claude: rate limits appear after the next Claude response (restart sessions).");
    println!("  Kimi:   auto-refreshed while abtop runs if you are logged in (`kimi login`).");
    println!(
        "          companion script: {}",
        kimi_usages_script_path_for(&kimi_dir()).display()
    );
}

fn setup_claude_statusline() {
    println!("[claude] StatusLine hook");

    let dir = claude_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("  ✗ failed to create {}: {}", dir.display(), e);
        eprintln!("    skipping Claude setup");
        return;
    }

    let script = script_path();
    match fs::write(&script, STATUSLINE_SCRIPT) {
        Ok(_) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
            }
            println!("  ✓ wrote {}", script.display());
        }
        Err(e) => {
            eprintln!("  ✗ failed to write {}: {}", script.display(), e);
            eprintln!("    skipping Claude setup");
            return;
        }
    }

    let settings_file = settings_path();
    let mut settings: Value = if settings_file.exists() {
        let content = match fs::read_to_string(&settings_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ✗ cannot read {}: {}", settings_file.display(), e);
                eprintln!("    skipping Claude settings update");
                return;
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
                eprintln!("    fix the file manually before re-running --setup");
                return;
            }
        }
    } else {
        Value::Object(Default::default())
    };

    let obj = settings.as_object_mut().unwrap();

    let expected_cmd = script.display().to_string();
    if let Some(existing) = obj.get("statusLine") {
        if let Some(existing_obj) = existing.as_object() {
            if let Some(cmd) = existing_obj.get("command") {
                let cmd_str = cmd.as_str().unwrap_or("");
                if cmd_str != expected_cmd && !cmd_str.is_empty() {
                    eprintln!("  ⚠ statusLine already configured: {}", cmd_str);
                    eprintln!("    leaving settings unchanged; script still written.");
                    eprintln!("    to use abtop's hook, point statusLine.command at:");
                    eprintln!("    {}", expected_cmd);
                    return;
                }
            }
        }
    }

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
        }
    }
}

fn setup_kimi_usages() {
    println!("\n[kimi] usages companion script");

    let dir = kimi_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("  ✗ failed to create {}: {}", dir.display(), e);
        eprintln!("    skipping Kimi setup");
        return;
    }

    match ensure_kimi_usages_script() {
        Some(script) => {
            println!("  ✓ wrote {}", script.display());
            println!(
                "    writes {} (abtop reads this file only)",
                kimi_rate_file_for(&dir).display()
            );
            if !kimi_has_credentials(&dir) {
                println!("  ⚠ not logged in yet — run `kimi login`, then start abtop");
                return;
            }
            // Synchronous first fetch so the panel has data immediately.
            run_companion_sync(&script, KIMI_COLD_START_TIMEOUT);
            if kimi_rate_file_for(&dir).is_file() {
                println!("  ✓ initial fetch succeeded");
            } else {
                println!("  ⚠ initial fetch failed or timed out (check `kimi login` / network)");
            }
        }
        None => {
            eprintln!("  ✗ failed to install Kimi companion script");
            eprintln!("    skipping Kimi setup");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn rate_file_missing_is_cold_not_fresh() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        assert!(!rate_file_is_fresh(&path));
        assert!(rate_file_is_cold(&path));
    }

    #[test]
    fn rate_file_just_written_is_fresh_not_cold() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fresh.json");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{{}}").unwrap();
        assert!(rate_file_is_fresh(&path));
        assert!(!rate_file_is_cold(&path));
    }

    #[cfg(unix)]
    #[test]
    fn wait_child_timeout_kills_slow_process() {
        // A process that would hang if not killed (`sleep` is unix-portable).
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let start = Instant::now();
        wait_child_with_timeout(&mut child, Duration::from_millis(200));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timeout path should return quickly"
        );
        // Child should be reaped.
        assert!(child.try_wait().ok().flatten().is_some());
    }
}
