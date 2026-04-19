use crate::model::RateLimitInfo;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// File written by the StatusLine hook: ~/.claude/abtop-rate-limits.json
const CLAUDE_RATE_FILE: &str = "abtop-rate-limits.json";

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
}

/// Read rate limit info from all known Claude config directories.
/// Checks the default ~/.claude, CLAUDE_CONFIG_DIR if set, and any
/// additional directories discovered from running Claude processes.
pub fn read_rate_limits(extra_dirs: &[PathBuf]) -> Vec<RateLimitInfo> {
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Collect candidate directories: defaults + discovered
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".claude"));
    }
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    dirs.extend_from_slice(extra_dirs);

    for dir in dirs {
        if !dir.is_dir() || !seen.insert(dir.clone()) {
            continue;
        }
        let path = dir.join(CLAUDE_RATE_FILE);
        if let Some(info) = read_rate_file(&path, "claude") {
            results.push(info);
        }
    }

    results
}

/// Read cached Codex rate limit (fallback when no live session provides one).
/// No staleness check — rate limits have their own `resets_at` expiry,
/// and the cache is updated whenever the next Codex session runs.
pub fn read_codex_cache() -> Option<RateLimitInfo> {
    let path = codex_cache_path()?;
    read_rate_file_impl(&path, "codex", false)
}

/// Write Codex rate limit to cache file (atomic: write temp + rename).
pub fn write_codex_cache(info: &RateLimitInfo) {
    let Some(path) = codex_cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let json = format!(
        r#"{{"source":"codex","five_hour":{},"seven_day":{},"updated_at":{}}}"#,
        window_json(info.five_hour_pct, info.five_hour_resets_at),
        window_json(info.seven_day_pct, info.seven_day_resets_at),
        info.updated_at.map(|v| v.to_string()).unwrap_or_else(|| "null".to_string()),
    );

    // Atomic write: temp file + rename to avoid corrupted reads
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn window_json(pct: Option<f64>, resets_at: Option<u64>) -> String {
    match (pct, resets_at) {
        (Some(p), Some(r)) => format!(r#"{{"used_percentage":{},"resets_at":{}}}"#, p, r),
        (Some(p), None) => format!(r#"{{"used_percentage":{},"resets_at":0}}"#, p),
        _ => "null".to_string(),
    }
}

fn codex_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("abtop").join(CODEX_CACHE_FILE))
}

fn read_rate_file(path: &Path, default_source: &str) -> Option<RateLimitInfo> {
    read_rate_file_impl(path, default_source, true)
}

fn read_rate_file_impl(path: &Path, default_source: &str, check_staleness: bool) -> Option<RateLimitInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let file: RateLimitFile = serde_json::from_str(&content).ok()?;

    // Ignore stale data (older than 10 minutes) when staleness check is enabled
    if check_staleness {
        if let Some(updated) = file.updated_at {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if now.saturating_sub(updated) > 600 {
                return None;
            }
        }
    }

    // Reject if both windows are absent
    if file.five_hour.is_none() && file.seven_day.is_none() {
        return None;
    }

    let source = if file.source.is_empty() {
        default_source.to_string()
    } else {
        file.source
    };

    Some(RateLimitInfo {
        source,
        five_hour_pct: file.five_hour.as_ref().map(|w| w.used_percentage),
        five_hour_resets_at: file.five_hour.as_ref().map(|w| w.resets_at),
        seven_day_pct: file.seven_day.as_ref().map(|w| w.used_percentage),
        seven_day_resets_at: file.seven_day.as_ref().map(|w| w.resets_at),
        updated_at: file.updated_at,
    })
}
