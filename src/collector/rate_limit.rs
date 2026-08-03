use super::codexbar::canonical_provider_id;
use crate::model::{RateLimitInfo, RateLimitProvenance, RateLimitWindow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// File written by the StatusLine hook: ~/.claude/abtop-rate-limits.json
const CLAUDE_RATE_FILE: &str = "abtop-rate-limits.json";

/// Cached Codex rate limit: ~/.cache/abtop/codex-rate-limits.json
const CODEX_CACHE_FILE: &str = "codex-rate-limits.json";

const MAX_RATE_LIMIT_FILE_BYTES: u64 = 64 * 1024;
const MAX_NATIVE_WINDOW_MINUTES: u64 = 365 * 24 * 60;

#[derive(Debug, Deserialize, Serialize)]
struct RateLimitFile {
    #[serde(default)]
    source: String,
    #[serde(default)]
    five_hour: Option<WindowInfo>,
    #[serde(default)]
    seven_day: Option<WindowInfo>,
    #[serde(default)]
    updated_at: Option<u64>,
    /// Native source-slot identities added by abtop's Codex cache. Older
    /// caches and Claude StatusLine files omit this field.
    #[serde(default)]
    windows: Vec<CachedNativeWindow>,
}

#[derive(Debug, Deserialize, Serialize)]
struct WindowInfo {
    #[serde(default)]
    used_percentage: f64,
    #[serde(default)]
    resets_at: u64,
    #[serde(default)]
    window_minutes: Option<u64>,
}

/// Content-free native Codex source slot persisted by abtop.
///
/// Labels and provenance are intentionally not serialized: both are derived
/// from the allowlisted slot and bounded duration when the cache is read.
#[derive(Debug, Deserialize, Serialize)]
struct CachedNativeWindow {
    id: String,
    used_percentage: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resets_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_minutes: Option<u64>,
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
    let _ = write_codex_cache_file(&path, info);
}

fn write_codex_cache_file(path: &Path, info: &RateLimitInfo) -> std::io::Result<()> {
    let cache = RateLimitFile {
        source: "codex".to_string(),
        five_hour: stored_legacy_window(
            info.five_hour_pct,
            info.five_hour_resets_at,
            info.five_hour_window_minutes,
        ),
        seven_day: stored_legacy_window(
            info.seven_day_pct,
            info.seven_day_resets_at,
            info.seven_day_window_minutes,
        ),
        updated_at: info.updated_at,
        windows: cached_native_windows(info),
    };
    let json = serde_json::to_vec(&cache).map_err(std::io::Error::other)?;

    // Atomic write: temp file + rename to avoid corrupted reads.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(tmp, path)
}

fn stored_legacy_window(
    used_percentage: Option<f64>,
    resets_at: Option<u64>,
    window_minutes: Option<u64>,
) -> Option<WindowInfo> {
    let used_percentage =
        used_percentage.filter(|value| value.is_finite() && (0.0..=100.0).contains(value))?;
    if window_minutes.is_some() && bounded_window_minutes(window_minutes).is_none() {
        return None;
    }
    Some(WindowInfo {
        used_percentage,
        resets_at: resets_at.unwrap_or(0),
        window_minutes: bounded_window_minutes(window_minutes),
    })
}

fn cached_native_windows(info: &RateLimitInfo) -> Vec<CachedNativeWindow> {
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|id| {
            let window = info.windows.iter().find(|window| {
                window.id == id && window.provenance == RateLimitProvenance::Native
            })?;
            if !window.used_pct.is_finite() || !(0.0..=100.0).contains(&window.used_pct) {
                return None;
            }
            if window.window_minutes.is_some()
                && bounded_window_minutes(window.window_minutes).is_none()
            {
                return None;
            }
            Some(CachedNativeWindow {
                id: id.to_string(),
                used_percentage: window.used_pct,
                resets_at: window.resets_at,
                window_minutes: window.window_minutes,
            })
        })
        .collect()
}

fn codex_cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("abtop").join(CODEX_CACHE_FILE))
}

fn read_rate_file(path: &Path, default_source: &str) -> Option<RateLimitInfo> {
    if std::fs::metadata(path).ok()?.len() > MAX_RATE_LIMIT_FILE_BYTES {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let file: RateLimitFile = serde_json::from_str(&content).ok()?;
    let five_hour = valid_legacy_window(file.five_hour.as_ref());
    let seven_day = valid_legacy_window(file.seven_day.as_ref());
    let mut windows = if default_source == "codex" {
        decoded_native_windows(&file.windows)
    } else {
        Vec::new()
    };
    if default_source == "codex" && windows.is_empty() && five_hour.is_none() {
        if let Some(window) = seven_day.filter(|window| {
            window
                .window_minutes
                .is_some_and(|minutes| minutes > 10_080)
        }) {
            if let Some(primary) = RateLimitWindow::try_new(
                "primary",
                native_window_label(window.window_minutes, "primary"),
                window.used_percentage,
                Some(window.resets_at),
                window.window_minutes,
                RateLimitProvenance::Native,
            ) {
                windows.push(primary);
            }
        }
    }

    // Reject if every legacy and source-slot window is absent.
    if five_hour.is_none() && seven_day.is_none() && windows.is_empty() {
        return None;
    }

    let trusted_source = canonical_provider_id(default_source)?;
    let source = canonical_provider_id(&file.source)
        .filter(|source| source == &trusted_source)
        .unwrap_or(trusted_source);

    Some(RateLimitInfo {
        source,
        five_hour_pct: five_hour.map(|w| w.used_percentage),
        five_hour_resets_at: five_hour.map(|w| w.resets_at),
        five_hour_window_minutes: five_hour
            .and_then(|w| w.window_minutes)
            .or(five_hour.map(|_| 300)),
        seven_day_pct: seven_day.map(|w| w.used_percentage),
        seven_day_resets_at: seven_day.map(|w| w.resets_at),
        seven_day_window_minutes: seven_day
            .and_then(|w| w.window_minutes)
            .or(seven_day.map(|_| 10_080)),
        updated_at: file.updated_at,
        windows,
    })
}

fn valid_legacy_window(window: Option<&WindowInfo>) -> Option<&WindowInfo> {
    let window = window?;
    if !window.used_percentage.is_finite()
        || !(0.0..=100.0).contains(&window.used_percentage)
        || (window.window_minutes.is_some()
            && bounded_window_minutes(window.window_minutes).is_none())
    {
        return None;
    }
    Some(window)
}

fn decoded_native_windows(windows: &[CachedNativeWindow]) -> Vec<RateLimitWindow> {
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|id| {
            let stored = windows.iter().find(|window| window.id == id)?;
            if !stored.used_percentage.is_finite()
                || !(0.0..=100.0).contains(&stored.used_percentage)
                || (stored.window_minutes.is_some()
                    && bounded_window_minutes(stored.window_minutes).is_none())
            {
                return None;
            }
            RateLimitWindow::try_new(
                id,
                native_window_label(stored.window_minutes, id),
                stored.used_percentage,
                stored.resets_at,
                stored.window_minutes,
                RateLimitProvenance::Native,
            )
        })
        .collect()
}

fn bounded_window_minutes(window_minutes: Option<u64>) -> Option<u64> {
    window_minutes.filter(|minutes| (1..=MAX_NATIVE_WINDOW_MINUTES).contains(minutes))
}

fn native_window_label(window_minutes: Option<u64>, slot: &str) -> String {
    match window_minutes {
        Some(minutes) if minutes % (24 * 60) == 0 => format!("{}d", minutes / (24 * 60)),
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes}m"),
        None if slot == "primary" => "Primary".to_string(),
        None => "Secondary".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_window(
        id: &str,
        used_pct: f64,
        resets_at: u64,
        window_minutes: u64,
    ) -> RateLimitWindow {
        RateLimitWindow::try_new(
            id,
            "ignored cache label",
            used_pct,
            Some(resets_at),
            Some(window_minutes),
            RateLimitProvenance::Native,
        )
        .unwrap()
    }

    #[test]
    fn codex_cache_round_trip_preserves_free_primary_source_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CODEX_CACHE_FILE);
        let original = RateLimitInfo {
            source: "codex".to_string(),
            seven_day_pct: Some(48.0),
            seven_day_resets_at: Some(1_780_000_000),
            seven_day_window_minutes: Some(43_200),
            updated_at: Some(1_750_000_000),
            windows: vec![native_window("primary", 48.0, 1_780_000_000, 43_200)],
            ..RateLimitInfo::default()
        };

        write_codex_cache_file(&path, &original).unwrap();
        let serialized = std::fs::read_to_string(&path).unwrap();
        assert!(!serialized.contains("ignored cache label"));
        assert!(!serialized.contains("provenance"));
        let restored = read_rate_file(&path, "codex").unwrap();

        assert_eq!(restored.seven_day_pct, Some(48.0));
        assert_eq!(restored.seven_day_window_minutes, Some(43_200));
        assert_eq!(restored.windows.len(), 1);
        assert_eq!(restored.windows[0].id, "primary");
        assert_eq!(restored.windows[0].label, "30d");
        assert_eq!(restored.windows[0].used_pct, 48.0);
        assert_eq!(restored.windows[0].window_minutes, Some(43_200));
        assert_eq!(restored.windows[0].provenance, RateLimitProvenance::Native);
    }

    #[test]
    fn codex_cache_round_trip_preserves_normal_secondary_source_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CODEX_CACHE_FILE);
        let original = RateLimitInfo {
            source: "codex".to_string(),
            seven_day_pct: Some(14.0),
            seven_day_resets_at: Some(1_775_186_466),
            seven_day_window_minutes: Some(10_080),
            updated_at: Some(1_750_000_000),
            windows: vec![native_window("secondary", 14.0, 1_775_186_466, 10_080)],
            ..RateLimitInfo::default()
        };

        write_codex_cache_file(&path, &original).unwrap();
        let restored = read_rate_file(&path, "codex").unwrap();

        assert_eq!(restored.windows.len(), 1);
        assert_eq!(restored.windows[0].id, "secondary");
        assert_eq!(restored.windows[0].label, "7d");
        assert_eq!(restored.windows[0].used_pct, 14.0);
        assert_eq!(restored.windows[0].window_minutes, Some(10_080));
    }

    #[test]
    fn legacy_codex_and_claude_files_remain_readable() {
        let dir = tempfile::tempdir().unwrap();
        let codex_path = dir.path().join("codex.json");
        std::fs::write(
            &codex_path,
            r#"{"source":"codex","five_hour":{"used_percentage":9.0,"resets_at":1774686045,"window_minutes":300},"seven_day":null,"updated_at":1750000000}"#,
        )
        .unwrap();
        let codex = read_rate_file(&codex_path, "codex").unwrap();
        assert_eq!(codex.five_hour_pct, Some(9.0));
        assert!(codex.windows.is_empty());

        let free_codex_path = dir.path().join("free-codex.json");
        std::fs::write(
            &free_codex_path,
            r#"{"source":"codex","five_hour":null,"seven_day":{"used_percentage":48.0,"resets_at":1780000000,"window_minutes":43200},"updated_at":1750000000}"#,
        )
        .unwrap();
        let free_codex = read_rate_file(&free_codex_path, "codex").unwrap();
        assert_eq!(free_codex.windows.len(), 1);
        assert_eq!(free_codex.windows[0].id, "primary");
        assert_eq!(free_codex.windows[0].label, "30d");

        let claude_path = dir.path().join("claude.json");
        std::fs::write(
            &claude_path,
            r#"{"five_hour":{"used_percentage":28.0,"resets_at":1774686045},"seven_day":{"used_percentage":6.0,"resets_at":1775186466}}"#,
        )
        .unwrap();
        let claude = read_rate_file(&claude_path, "claude").unwrap();
        assert_eq!(claude.source, "claude");
        assert_eq!(claude.five_hour_window_minutes, Some(300));
        assert_eq!(claude.seven_day_window_minutes, Some(10_080));
        assert!(claude.windows.is_empty());
    }

    #[test]
    fn source_must_match_the_canonical_trusted_default() {
        let dir = tempfile::tempdir().unwrap();
        let normalized_path = dir.path().join("normalized.json");
        std::fs::write(
            &normalized_path,
            r#"{"source":"CoDeX","five_hour":{"used_percentage":1.0,"resets_at":1}}"#,
        )
        .unwrap();
        assert_eq!(
            read_rate_file(&normalized_path, "codex").unwrap().source,
            "codex"
        );

        let mismatched_path = dir.path().join("mismatched.json");
        std::fs::write(
            &mismatched_path,
            r#"{"source":"grok","five_hour":{"used_percentage":1.0,"resets_at":1}}"#,
        )
        .unwrap();
        assert_eq!(
            read_rate_file(&mismatched_path, "codex").unwrap().source,
            "codex"
        );

        let unsafe_path = dir.path().join("unsafe.json");
        std::fs::write(
            &unsafe_path,
            r#"{"source":"grok\nprivate","five_hour":{"used_percentage":1.0,"resets_at":1}}"#,
        )
        .unwrap();
        assert_eq!(
            read_rate_file(&unsafe_path, "codex").unwrap().source,
            "codex"
        );
    }

    #[test]
    fn invalid_legacy_duration_is_not_rewritten_as_a_default_window() {
        assert!(stored_legacy_window(Some(1.0), Some(10), Some(u64::MAX)).is_none());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid-duration.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"source":"codex","five_hour":{{"used_percentage":1.0,"resets_at":1,"window_minutes":{}}}}}"#,
                u64::MAX
            ),
        )
        .unwrap();
        assert!(read_rate_file(&path, "codex").is_none());
    }
}
