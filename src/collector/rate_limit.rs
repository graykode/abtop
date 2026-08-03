use crate::model::RateLimitInfo;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Shared filename written by companion hooks for Claude and Kimi.
/// Claude: `~/.claude/abtop-rate-limits.json` (StatusLine)
/// Kimi:   `~/.kimi-code/abtop-rate-limits.json` (`abtop --setup` usages script)
const RATE_LIMIT_FILE: &str = "abtop-rate-limits.json";

/// Cached Codex rate limit: ~/.cache/abtop/codex-rate-limits.json
const CODEX_CACHE_FILE: &str = "codex-rate-limits.json";

#[derive(Debug, Deserialize)]
struct RateLimitFile {
    #[serde(default)]
    source: String,
    #[serde(default)]
    five_hour: Option<WindowInfo>,
    #[serde(default)]
    seven_day: Option<WindowInfo>,
    #[serde(default)]
    updated_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WindowInfo {
    #[serde(default)]
    used_percentage: f64,
    #[serde(default)]
    resets_at: u64,
    #[serde(default)]
    window_minutes: Option<u64>,
}

/// Read account-level rate limits from local hook files only.
///
/// - Claude: `~/.claude/abtop-rate-limits.json` (+ CLAUDE_CONFIG_DIR / discovered dirs)
/// - Kimi:   `~/.kimi-code/abtop-rate-limits.json` (+ KIMI_CODE_HOME)
///
/// abtop never contacts provider APIs. Companion scripts write these files:
/// Claude StatusLine, and for Kimi an auto-spawned `abtop-usages.sh` when
/// credentials exist (also installable via `abtop --setup`).
pub fn read_rate_limits(extra_dirs: &[PathBuf]) -> Vec<RateLimitInfo> {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Claude config roots
    let mut claude_dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        claude_dirs.push(home.join(".claude"));
    }
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        claude_dirs.push(PathBuf::from(dir));
    }
    claude_dirs.extend_from_slice(extra_dirs);

    for dir in claude_dirs {
        if !dir.is_dir() || !seen.insert(dir.clone()) {
            continue;
        }
        let path = dir.join(RATE_LIMIT_FILE);
        if let Some(info) = read_rate_file(&path, "claude") {
            results.push(info);
        }
    }

    // Kimi Code config root (local file written by abtop-usages.sh)
    for dir in kimi_config_dirs() {
        if !dir.is_dir() || !seen.insert(dir.clone()) {
            continue;
        }
        let path = dir.join(RATE_LIMIT_FILE);
        if let Some(info) = read_rate_file(&path, "kimi") {
            results.push(info);
        }
    }

    results
}

fn kimi_config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var("KIMI_CODE_HOME") {
        let p = PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            dirs.push(p);
        }
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".kimi-code"));
        // Legacy kimi-cli home, if a user points the companion script there.
        dirs.push(home.join(".kimi"));
    }
    dirs
}

/// Read cached Codex rate limit (fallback when no live session provides one).
/// Rate limits have their own `resets_at` expiry and the cache is refreshed
/// whenever the next Codex session runs, so the reader keeps serving the last
/// known value regardless of file age — the UI shows "N m ago" for staleness.
pub fn read_codex_cache() -> Option<RateLimitInfo> {
    let path = codex_cache_path()?;
    read_rate_file(&path, "codex")
}

/// Write Codex rate limit to cache file (atomic: write temp + rename).
pub fn write_codex_cache(info: &RateLimitInfo) {
    let Some(path) = codex_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let json = format!(
        r#"{{"source":"codex","five_hour":{},"seven_day":{},"updated_at":{}}}"#,
        window_json(
            info.five_hour_pct,
            info.five_hour_resets_at,
            info.five_hour_window_minutes
        ),
        window_json(
            info.seven_day_pct,
            info.seven_day_resets_at,
            info.seven_day_window_minutes
        ),
        info.updated_at
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".to_string()),
    );

    // Atomic write: temp file + rename to avoid corrupted reads
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn window_json(pct: Option<f64>, resets_at: Option<u64>, window_minutes: Option<u64>) -> String {
    match (pct, resets_at) {
        (Some(p), Some(r)) => match window_minutes {
            Some(m) => format!(
                r#"{{"used_percentage":{},"resets_at":{},"window_minutes":{}}}"#,
                p, r, m
            ),
            None => format!(r#"{{"used_percentage":{},"resets_at":{}}}"#, p, r),
        },
        (Some(p), None) => match window_minutes {
            Some(m) => format!(
                r#"{{"used_percentage":{},"resets_at":0,"window_minutes":{}}}"#,
                p, m
            ),
            None => format!(r#"{{"used_percentage":{},"resets_at":0}}"#, p),
        },
        _ => "null".to_string(),
    }
}

fn codex_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("abtop").join(CODEX_CACHE_FILE))
}

fn read_rate_file(path: &Path, default_source: &str) -> Option<RateLimitInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let file: RateLimitFile = serde_json::from_str(&content).ok()?;

    // Reject if both windows are absent
    if file.five_hour.is_none() && file.seven_day.is_none() {
        return None;
    }

    let source = if file.source.is_empty() {
        default_source.to_string()
    } else {
        file.source
    };

    // Default window lengths when the hook omits window_minutes:
    // Claude/Codex-style 5h + 7d; Kimi short window is also 5h (300 min).
    let default_short = 300;
    let default_long = if source.eq_ignore_ascii_case("kimi") {
        // Kimi's top-level `usage` window is account-period, not fixed 7d.
        None
    } else {
        Some(10_080)
    };

    Some(RateLimitInfo {
        source,
        five_hour_pct: file.five_hour.as_ref().map(|w| w.used_percentage),
        five_hour_resets_at: file.five_hour.as_ref().map(|w| w.resets_at),
        five_hour_window_minutes: file
            .five_hour
            .as_ref()
            .and_then(|w| w.window_minutes)
            .or(file.five_hour.as_ref().map(|_| default_short)),
        seven_day_pct: file.seven_day.as_ref().map(|w| w.used_percentage),
        seven_day_resets_at: file.seven_day.as_ref().map(|w| w.resets_at),
        seven_day_window_minutes: file
            .seven_day
            .as_ref()
            .and_then(|w| w.window_minutes)
            .or(file.seven_day.as_ref().and(default_long)),
        updated_at: file.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn reads_kimi_hook_file_from_kimi_code_home() {
        let dir = tempdir().unwrap();
        let kimi_home = dir.path().join(".kimi-code");
        std::fs::create_dir_all(&kimi_home).unwrap();
        let path = kimi_home.join(RATE_LIMIT_FILE);
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"{{"source":"kimi","five_hour":{{"used_percentage":34.0,"resets_at":1785746704,"window_minutes":300}},"seven_day":{{"used_percentage":31.0,"resets_at":1786067104}},"updated_at":1785739503}}"#
        )
        .unwrap();

        // Temporarily point home-like discovery via KIMI_CODE_HOME.
        let prev = std::env::var_os("KIMI_CODE_HOME");
        std::env::set_var("KIMI_CODE_HOME", &kimi_home);
        let results = read_rate_limits(&[]);
        match prev {
            Some(v) => std::env::set_var("KIMI_CODE_HOME", v),
            None => std::env::remove_var("KIMI_CODE_HOME"),
        }

        let kimi = results
            .iter()
            .find(|r| r.source.eq_ignore_ascii_case("kimi"))
            .expect("kimi rate limit present");
        assert!((kimi.five_hour_pct.unwrap() - 34.0).abs() < 0.01);
        assert_eq!(kimi.five_hour_window_minutes, Some(300));
        assert!((kimi.seven_day_pct.unwrap() - 31.0).abs() < 0.01);
        assert!(kimi.seven_day_window_minutes.is_none());
    }
}
