use crate::model::RateLimitInfo;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Shared filename written by the Claude hook and optional Kimi companion.
const RATE_LIMIT_FILE: &str = "abtop-rate-limits.json";

/// Cached Codex rate limit: ~/.cache/abtop/codex-rate-limits.json
const CODEX_CACHE_FILE: &str = "codex-rate-limits.json";

/// Provider-written hook data older than this is no longer reliable.
const HOOK_STALE_SECS: u64 = 600;

#[derive(Debug, Deserialize)]
struct RateLimitFile {
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

/// Read account-level rate limits from local cache files.
///
/// Claude data comes from its StatusLine hook. Kimi data comes from the
/// optional companion installed by `abtop --setup`; this reader never performs
/// network or credential operations itself.
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
        let path = dir.join(RATE_LIMIT_FILE);
        if let Some(info) = read_rate_file(&path, "claude", true) {
            results.push(info);
        }
    }

    // Keep the last valid Kimi value visible when a refresh fails. The quota
    // panel dims cache data older than ten minutes, so users can distinguish a
    // stale value without losing the last known account state entirely.
    for dir in kimi_config_dirs() {
        if !dir.is_dir() || !seen.insert(dir.clone()) {
            continue;
        }
        let path = dir.join(RATE_LIMIT_FILE);
        if let Some(info) = read_rate_file(&path, "kimi", false) {
            results.push(info);
        }
    }

    results
}

fn kimi_config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var("KIMI_CODE_HOME") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            dirs.push(path);
        }
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".kimi-code"));
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
    read_rate_file(&path, "codex", false)
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

fn read_rate_file(path: &Path, default_source: &str, reject_stale: bool) -> Option<RateLimitInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let file: RateLimitFile = serde_json::from_str(&content).ok()?;

    if reject_stale
        && file.updated_at.is_some_and(|updated_at| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            now.saturating_sub(updated_at) > HOOK_STALE_SECS
        })
    {
        return None;
    }

    // Reject if both windows are absent
    if file.five_hour.is_none() && file.seven_day.is_none() {
        return None;
    }

    // The cache location determines the provider. Do not allow malformed or
    // hand-edited JSON to impersonate another quota column via `source`.
    let source = default_source.to_string();

    // Kimi's long window is the account billing/subscription period, not a
    // fixed seven-day window. Leave it unlengthed unless the companion reports
    // a concrete duration so the UI can label it as the plan period.
    let default_long_window = if source.eq_ignore_ascii_case("kimi") {
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
            .or(file.five_hour.as_ref().map(|_| 300)),
        seven_day_pct: file.seven_day.as_ref().map(|w| w.used_percentage),
        seven_day_resets_at: file.seven_day.as_ref().map(|w| w.resets_at),
        seven_day_window_minutes: file
            .seven_day
            .as_ref()
            .and_then(|w| w.window_minutes)
            .or(file.seven_day.as_ref().and(default_long_window)),
        updated_at: file.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_rate_file(path: &Path, updated_at: u64) {
        std::fs::write(
            path,
            format!(
                r#"{{"source":"claude","five_hour":{{"used_percentage":25.0,"resets_at":0}},"updated_at":{updated_at}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn rejects_stale_hook_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(RATE_LIMIT_FILE);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        write_rate_file(&path, now.saturating_sub(HOOK_STALE_SECS + 1));

        assert!(read_rate_file(&path, "claude", true).is_none());
    }

    #[test]
    fn codex_cache_can_keep_stale_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(CODEX_CACHE_FILE);
        write_rate_file(&path, 1);

        assert!(read_rate_file(&path, "codex", false).is_some());
    }

    #[test]
    fn kimi_cache_keeps_stale_data_and_does_not_invent_a_weekly_window() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(RATE_LIMIT_FILE);
        std::fs::write(
            &path,
            r#"{"source":"kimi","five_hour":{"used_percentage":40.0,"resets_at":10,"window_minutes":300},"seven_day":{"used_percentage":32.0,"resets_at":20},"updated_at":1}"#,
        )
        .unwrap();

        let info = read_rate_file(&path, "kimi", false).expect("stale Kimi cache is retained");
        assert_eq!(info.five_hour_pct, Some(40.0));
        assert_eq!(info.five_hour_window_minutes, Some(300));
        assert_eq!(info.seven_day_pct, Some(32.0));
        assert_eq!(info.seven_day_window_minutes, None);
        assert!(read_rate_file(&path, "kimi", true).is_none());
    }
}
