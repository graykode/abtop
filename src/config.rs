use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy)]
pub struct PanelVisibility {
    pub context: bool,
    pub quota: bool,
    pub tokens: bool,
    pub projects: bool,
    pub ports: bool,
    pub sessions: bool,
    pub mcp: bool,
}

impl Default for PanelVisibility {
    fn default() -> Self {
        Self {
            context: true,
            quota: true,
            tokens: true,
            projects: true,
            ports: true,
            sessions: true,
            mcp: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomThemeConfig {
    pub name: String,
    pub values: BTreeMap<String, String>,
}

pub struct AppConfig {
    pub theme: String,
    /// Agent CLI names to exclude from the TUI (e.g. ["codex"] to hide Codex).
    /// Matched case-insensitively against each collector's agent_cli identifier.
    pub hidden_agents: Vec<String>,
    /// Additional Claude config directories to scan for sessions.
    /// Useful for multi-profile setups that use separate CLAUDE_CONFIG_DIR roots.
    pub claude_config_dirs: Vec<PathBuf>,
    pub panels: PanelVisibility,
    /// UI language override. Empty string means auto-detect from `LANG`.
    /// Recognized values: "en", "zh" (anything starting with "zh" maps to Simplified Chinese).
    pub language: String,
    /// User-defined theme sections from `[themes.<name>]`. Values remain raw
    /// so the theme module can validate color-specific syntax.
    pub custom_themes: Vec<CustomThemeConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: "btop".to_string(),
            hidden_agents: Vec::new(),
            claude_config_dirs: Vec::new(),
            panels: PanelVisibility::default(),
            language: String::new(),
            custom_themes: Vec::new(),
        }
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("abtop").join("config.toml"))
}

pub fn load_config() -> AppConfig {
    let path = match config_path() {
        Some(p) => p,
        None => return AppConfig::default(),
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return AppConfig::default(),
    };

    parse_config_body(&content)
}

fn parse_config_body(content: &str) -> AppConfig {
    let mut config = AppConfig::default();
    let mut current_custom_theme: Option<String> = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        if let Some(section) = parse_section_header(line) {
            current_custom_theme = section
                .strip_prefix("themes.")
                .filter(|name| !name.trim().is_empty())
                .map(|name| name.trim().to_string());
            continue;
        }

        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            let val = strip_inline_comment(val.trim());

            if let Some(name) = &current_custom_theme {
                if !key.is_empty() {
                    custom_theme_mut(&mut config.custom_themes, name)
                        .values
                        .insert(key.to_string(), val.to_string());
                }
                continue;
            }

            if key == "hidden_agents" {
                config.hidden_agents = parse_string_array(val);
                continue;
            }
            if key == "claude_config_dirs" {
                config.claude_config_dirs = parse_path_array(val);
                continue;
            }
            let val = val.trim_matches('\"').trim_matches('\'');
            match key {
                "theme" => config.theme = val.to_string(),
                "language" => config.language = val.to_string(),
                "show_context" => config.panels.context = parse_bool(val).unwrap_or(true),
                "show_quota" => config.panels.quota = parse_bool(val).unwrap_or(true),
                "show_tokens" => config.panels.tokens = parse_bool(val).unwrap_or(true),
                "show_projects" => config.panels.projects = parse_bool(val).unwrap_or(true),
                "show_ports" => config.panels.ports = parse_bool(val).unwrap_or(true),
                "show_sessions" => config.panels.sessions = parse_bool(val).unwrap_or(true),
                "show_mcp" => config.panels.mcp = parse_bool(val).unwrap_or(true),
                _ => {}
            }
        }
    }
    config
}

fn parse_section_header(line: &str) -> Option<&str> {
    let line = strip_inline_comment(line);
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner)
    }
}

fn custom_theme_mut<'a>(
    themes: &'a mut Vec<CustomThemeConfig>,
    name: &str,
) -> &'a mut CustomThemeConfig {
    if let Some(pos) = themes.iter().position(|theme| theme.name == name) {
        return &mut themes[pos];
    }
    themes.push(CustomThemeConfig {
        name: name.to_string(),
        values: BTreeMap::new(),
    });
    themes.last_mut().expect("just pushed custom theme")
}

fn strip_inline_comment(raw: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for (idx, ch) in raw.char_indices() {
        match ch {
            '\\' if in_double => {
                escaped = !escaped;
                continue;
            }
            '"' if !in_single && !escaped => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return raw[..idx].trim(),
            _ => {}
        }
        escaped = false;
    }

    raw.trim()
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Parse a simple one-line TOML string array like `["a", "b"]`.
/// Returns an empty Vec for malformed input to keep config loading infallible.
fn parse_string_array(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return Vec::new();
    };
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_path_array(raw: &str) -> Vec<PathBuf> {
    parse_string_array(raw)
        .into_iter()
        .map(|s| expand_home_path(&s))
        .collect()
}

fn expand_home_path(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

pub fn save_theme(name: &str) -> Result<(), String> {
    write_with_updates(&[("theme", format!("\"{}\"", name))])
}

pub fn save_panel_visibility(panels: &PanelVisibility) -> Result<(), String> {
    write_with_updates(&[
        ("show_context", panels.context.to_string()),
        ("show_quota", panels.quota.to_string()),
        ("show_tokens", panels.tokens.to_string()),
        ("show_projects", panels.projects.to_string()),
        ("show_ports", panels.ports.to_string()),
        ("show_sessions", panels.sessions.to_string()),
        ("show_mcp", panels.mcp.to_string()),
    ])
}

/// Read the config, replace or append each (key, value) pair, write it back.
/// Lines that don't match any key are preserved verbatim so unknown keys and
/// comments survive saves driven by unrelated parts of the UI.
fn write_with_updates(updates: &[(&str, String)]) -> Result<(), String> {
    let path = config_path().ok_or("no config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let new_content = rewrite_kv_lines(&content, updates);
    std::fs::write(&path, new_content).map_err(|e| e.to_string())
}

/// Rewrite (or append) the listed `key = value` lines in a config body.
/// Every other line is preserved verbatim so keys set by the user or by a
/// different save_* helper survive.
fn rewrite_kv_lines(content: &str, updates: &[(&str, String)]) -> String {
    let mut found = vec![false; updates.len()];
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut appended_missing = false;

    for line in content.lines() {
        let starts_section = parse_section_header(line.trim()).is_some();
        if starts_section && !appended_missing {
            append_missing_updates(&mut out, updates, &found);
            appended_missing = true;
        }

        let line_key = if in_section {
            None
        } else {
            line.split_once('=').map(|(k, _)| k.trim().to_string())
        };
        let mut replaced = false;
        if let Some(key) = line_key {
            if let Some(idx) = updates.iter().position(|(k, _)| *k == key) {
                out.push(format!("{} = {}", updates[idx].0, updates[idx].1));
                found[idx] = true;
                replaced = true;
            }
        }
        if !replaced {
            out.push(line.to_string());
        }
        if starts_section {
            in_section = true;
        }
    }

    if !appended_missing {
        append_missing_updates(&mut out, updates, &found);
    }

    out.join("\n") + "\n"
}

fn append_missing_updates(out: &mut Vec<String>, updates: &[(&str, String)], found: &[bool]) {
    for (idx, (k, v)) in updates.iter().enumerate() {
        if !found[idx] {
            out.push(format!("{} = {}", k, v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_array_basic() {
        assert_eq!(parse_string_array(r#"["codex"]"#), vec!["codex"]);
        assert_eq!(
            parse_string_array(r#"["codex", "claude"]"#),
            vec!["codex", "claude"]
        );
    }

    #[test]
    fn parse_string_array_quote_styles_and_whitespace() {
        assert_eq!(
            parse_string_array(r#"[ 'codex' , "claude" ]"#),
            vec!["codex", "claude"]
        );
    }

    #[test]
    fn parse_string_array_empty_and_malformed() {
        assert!(parse_string_array("[]").is_empty());
        assert!(parse_string_array("not an array").is_empty());
        assert!(parse_string_array(r#"["a",,]"#)
            .iter()
            .all(|s| !s.is_empty()));
    }

    #[test]
    fn parse_path_array_expands_home_relative_entries() {
        let home = dirs::home_dir().unwrap();
        let paths = parse_path_array(r#"["~/.claude-personal", "/tmp/.claude-work"]"#);

        assert_eq!(paths[0], home.join(".claude-personal"));
        assert_eq!(paths[1], PathBuf::from("/tmp/.claude-work"));
    }

    #[test]
    fn parse_config_body_loads_claude_config_dirs() {
        let home = dirs::home_dir().unwrap();
        let cfg = parse_config_body(r#"claude_config_dirs = ["~/.claude-personal"]"#);

        assert_eq!(cfg.claude_config_dirs, vec![home.join(".claude-personal")]);
    }

    #[test]
    fn parse_config_body_loads_custom_theme_sections() {
        let cfg = parse_config_body(
            r##"
theme = "midnight" # active custom theme
[themes.midnight]
base = "dracula"
main_bg = "#101010"
cpu_grad = ["#000000", "#111111", "#ffffff"]
"##,
        );

        assert_eq!(cfg.theme, "midnight");
        assert_eq!(cfg.custom_themes.len(), 1);
        assert_eq!(cfg.custom_themes[0].name, "midnight");
        assert_eq!(
            cfg.custom_themes[0]
                .values
                .get("main_bg")
                .map(String::as_str),
            Some("\"#101010\"")
        );
        assert_eq!(
            cfg.custom_themes[0]
                .values
                .get("cpu_grad")
                .map(String::as_str),
            Some("[\"#000000\", \"#111111\", \"#ffffff\"]")
        );
    }

    #[test]
    fn strip_inline_comment_preserves_hash_inside_quotes() {
        assert_eq!(
            strip_inline_comment(r##""#101010" # color"##),
            r##""#101010""##
        );
        assert_eq!(strip_inline_comment("true # comment"), "true");
    }

    fn theme_update(name: &str) -> Vec<(&'static str, String)> {
        vec![("theme", format!("\"{}\"", name))]
    }

    #[test]
    fn rewrite_theme_preserves_hidden_agents_line() {
        let before = "theme = \"btop\"\nhidden_agents = [\"codex\"]\n";
        let after = rewrite_kv_lines(before, &theme_update("dracula"));
        assert!(after.contains("theme = \"dracula\""));
        assert!(
            after.contains("hidden_agents = [\"codex\"]"),
            "hidden_agents line dropped:\n{after}"
        );
    }

    #[test]
    fn rewrite_theme_preserves_arbitrary_unknown_keys() {
        let before = "# user comment\nfuture_key = 42\ntheme = \"btop\"\n";
        let after = rewrite_kv_lines(before, &theme_update("nord"));
        assert!(after.contains("# user comment"));
        assert!(after.contains("future_key = 42"));
        assert!(after.contains("theme = \"nord\""));
    }

    #[test]
    fn rewrite_theme_appends_when_missing() {
        let before = "hidden_agents = [\"codex\"]\n";
        let after = rewrite_kv_lines(before, &theme_update("gruvbox"));
        assert!(after.contains("hidden_agents = [\"codex\"]"));
        assert!(after.contains("theme = \"gruvbox\""));
    }

    #[test]
    fn rewrite_theme_appends_before_custom_theme_section() {
        let before = "[themes.midnight]\nmain_bg = \"#101010\"\n";
        let after = rewrite_kv_lines(before, &theme_update("midnight"));
        assert!(after.starts_with("theme = \"midnight\"\n[themes.midnight]"));
        assert!(after.contains("main_bg = \"#101010\""));
    }

    #[test]
    fn rewrite_theme_ignores_theme_keys_inside_sections() {
        let before = "theme = \"btop\"\n[themes.midnight]\ntheme = \"not-a-root-key\"\n";
        let after = rewrite_kv_lines(before, &theme_update("dracula"));
        assert!(after.contains("theme = \"dracula\"\n[themes.midnight]"));
        assert!(after.contains("theme = \"not-a-root-key\""));
    }

    #[test]
    fn rewrite_panels_replaces_existing_and_appends_missing() {
        let before = "theme = \"btop\"\nshow_quota = true\n";
        let updates: Vec<(&str, String)> = vec![
            ("show_quota", "false".to_string()),
            ("show_projects", "false".to_string()),
        ];
        let after = rewrite_kv_lines(before, &updates);
        assert!(after.contains("show_quota = false"));
        assert!(!after.contains("show_quota = true"));
        assert!(after.contains("show_projects = false"));
        assert!(after.contains("theme = \"btop\""));
    }

    #[test]
    fn parse_bool_round_trips_visibility_keys() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("False"), Some(false));
        assert_eq!(parse_bool("nope"), None);
    }

    #[test]
    fn rewrite_language_replaces_existing() {
        let before = "theme = \"btop\"\nlanguage = \"en\"\n";
        let updates: Vec<(&str, String)> = vec![("language", "\"zh\"".to_string())];
        let after = rewrite_kv_lines(before, &updates);
        assert!(after.contains("language = \"zh\""));
        assert!(!after.contains("language = \"en\""));
        assert!(after.contains("theme = \"btop\""));
    }
}
