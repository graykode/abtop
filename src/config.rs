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

#[derive(Clone, Copy)]
pub struct ControlPolicy {
    pub allow_kill_sessions: bool,
    pub allow_kill_orphan_ports: bool,
    /// Allow dispatching a one-shot non-interactive prompt to a Claude Code
    /// agent. Opt-in (default `false`) because dispatch is a mutating action
    /// that crosses the read-only boundary; the UI surface for it
    /// (`P6-UX-01`) is still pending.
    pub allow_dispatch_claude: bool,
    /// Allow dispatching a one-shot non-interactive prompt to a Codex CLI
    /// agent. Opt-in (default `false`); see `allow_dispatch_claude`.
    pub allow_dispatch_codex: bool,
    /// Allow dispatching a one-shot non-interactive prompt to an OpenCode
    /// agent. Opt-in (default `false`); see `allow_dispatch_claude`.
    pub allow_dispatch_opencode: bool,
}

impl Default for ControlPolicy {
    fn default() -> Self {
        Self {
            allow_kill_sessions: true,
            allow_kill_orphan_ports: true,
            allow_dispatch_claude: false,
            allow_dispatch_codex: false,
            allow_dispatch_opencode: false,
        }
    }
}

impl ControlPolicy {
    /// Whether dispatch is allowed for a given collector `agent_cli` identifier.
    /// Match is case-insensitive and tolerant of common synonyms
    /// (`claude`/`claude-code`, `codex`/`codex-cli`, `opencode`).
    ///
    /// Pre-staged for `P6-UX-01`; no production caller yet.
    #[allow(dead_code)]
    pub fn is_dispatch_allowed(&self, agent_cli: &str) -> bool {
        match agent_cli.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claude_code" | "cc" => self.allow_dispatch_claude,
            "codex" | "codex-cli" | "codex_cli" => self.allow_dispatch_codex,
            "opencode" | "open-code" | "open_code" => self.allow_dispatch_opencode,
            _ => false,
        }
    }
}

pub struct AppConfig {
    pub theme: String,
    /// Agent CLI names to exclude from the TUI (e.g. ["codex"] to hide Codex).
    /// Matched case-insensitively against each collector's agent_cli identifier.
    pub hidden_agents: Vec<String>,
    pub panels: PanelVisibility,
    pub control_policy: ControlPolicy,
    /// UI language override. English is currently the supported UI language.
    /// Kept for config-file compatibility with earlier releases.
    pub language: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: "btop".to_string(),
            hidden_agents: Vec::new(),
            panels: PanelVisibility::default(),
            control_policy: ControlPolicy::default(),
            language: String::new(),
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

    parse_config_content(&content)
}

fn parse_config_content(content: &str) -> AppConfig {
    let mut config = AppConfig::default();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            // Strip quotes (double or single) and inline comments
            let val = val.trim();
            let val = if let Some(comment_pos) = val.find('#') {
                val[..comment_pos].trim()
            } else {
                val
            };
            if key == "hidden_agents" {
                config.hidden_agents = parse_string_array(val);
                continue;
            }
            let val = val.trim_matches('"').trim_matches('\'');
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
                "allow_kill_sessions" => {
                    config.control_policy.allow_kill_sessions = parse_bool(val).unwrap_or(true)
                }
                "allow_kill_orphan_ports" => {
                    config.control_policy.allow_kill_orphan_ports = parse_bool(val).unwrap_or(true)
                }
                "allow_dispatch_claude" => {
                    config.control_policy.allow_dispatch_claude = parse_bool(val).unwrap_or(false)
                }
                "allow_dispatch_codex" => {
                    config.control_policy.allow_dispatch_codex = parse_bool(val).unwrap_or(false)
                }
                "allow_dispatch_opencode" => {
                    config.control_policy.allow_dispatch_opencode = parse_bool(val).unwrap_or(false)
                }
                _ => {}
            }
        }
    }
    config
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
    for line in content.lines() {
        let line_key = line.split_once('=').map(|(k, _)| k.trim().to_string());
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
    }
    for (idx, (k, v)) in updates.iter().enumerate() {
        if !found[idx] {
            out.push(format!("{} = {}", k, v));
        }
    }
    out.join("\n") + "\n"
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
    fn parse_control_policy_keys() {
        let config =
            parse_config_content("allow_kill_sessions = false\nallow_kill_orphan_ports = false\n");

        assert!(!config.control_policy.allow_kill_sessions);
        assert!(!config.control_policy.allow_kill_orphan_ports);
    }

    #[test]
    fn dispatch_flags_default_false() {
        let config = AppConfig::default();
        assert!(!config.control_policy.allow_dispatch_claude);
        assert!(!config.control_policy.allow_dispatch_codex);
        assert!(!config.control_policy.allow_dispatch_opencode);
    }

    #[test]
    fn parse_dispatch_policy_keys() {
        let config = parse_config_content(
            "allow_dispatch_claude = true\nallow_dispatch_codex = true\nallow_dispatch_opencode = true\n",
        );

        assert!(config.control_policy.allow_dispatch_claude);
        assert!(config.control_policy.allow_dispatch_codex);
        assert!(config.control_policy.allow_dispatch_opencode);
    }

    #[test]
    fn is_dispatch_allowed_matches_agent_cli_synonyms() {
        let mut policy = ControlPolicy::default();
        assert!(!policy.is_dispatch_allowed("claude"));
        assert!(!policy.is_dispatch_allowed("codex-cli"));

        policy.allow_dispatch_claude = true;
        assert!(policy.is_dispatch_allowed("claude"));
        assert!(policy.is_dispatch_allowed("Claude-Code"));
        assert!(policy.is_dispatch_allowed("claude_code"));
        assert!(!policy.is_dispatch_allowed("codex"));

        policy.allow_dispatch_codex = true;
        assert!(policy.is_dispatch_allowed("codex"));
        assert!(policy.is_dispatch_allowed("Codex-CLI"));

        policy.allow_dispatch_opencode = true;
        assert!(policy.is_dispatch_allowed("opencode"));
        assert!(policy.is_dispatch_allowed("Open-Code"));

        assert!(!policy.is_dispatch_allowed("gemini"));
        assert!(!policy.is_dispatch_allowed(""));
    }

    #[test]
    fn rewrite_language_replaces_existing() {
        let before = "theme = \"btop\"\nlanguage = \"en\"\n";
        let updates: Vec<(&str, String)> = vec![("language", "\"en\"".to_string())];
        let after = rewrite_kv_lines(before, &updates);
        assert!(after.contains("language = \"en\""));
        assert!(after.contains("theme = \"btop\""));
    }
}
