use std::sync::LazyLock;

static LOCALE_EN: LazyLock<std::collections::HashMap<&str, &str>> = LazyLock::new(|| {
    let mut m = std::collections::HashMap::new();

    // Status icons
    m.insert("sess.think", "◉ Think");
    m.insert("sess.exec", "● Exec");
    m.insert("sess.wait", "◌ Wait");
    m.insert("sess.rate", "⏳ Rate");
    m.insert("sess.done", "✓ Done");

    // Column headers
    m.insert("col.ai", "AI");
    m.insert("col.pid", "Pid");
    m.insert("col.project", "Project");
    m.insert("col.session", "Session");
    m.insert("col.sess", "Sess");
    m.insert("col.summary", "Summary");
    m.insert("col.status", "Status");
    m.insert("col.model", "Model");
    m.insert("col.context", "Context");
    m.insert("col.ctx", "Ctx");
    m.insert("col.tokens", "Tokens");
    m.insert("col.memory", "Memory");
    m.insert("col.turn", "Turn");

    // Agent labels
    m.insert("agent.claude", "*CC");
    m.insert("agent.codex", ">CD");

    // Tool labels
    m.insert("tool.bash", "Bash");
    m.insert("tool.read", "Read");
    m.insert("tool.write", "Write");
    m.insert("tool.edit", "Edit");
    m.insert("tool.glob", "Glob");
    m.insert("tool.grep", "Grep");
    m.insert("tool.brexec", "Brexec");
    m.insert("tool.web_search", "WebSearch");
    m.insert("tool.web_fetch", "WebFetch");
    m.insert("tool.tdd", "TDD");
    m.insert("tool.investigate", "Investigate");
    m.insert("tool.lsp", "LSP");
    m.insert("tool.notebook_edit", "Notebook");
    m.insert("tool.task_create", "TaskCreate");
    m.insert("tool.task_update", "TaskUpdate");
    m.insert("tool.task_list", "TaskList");
    m.insert("tool.task_get", "TaskGet");
    m.insert("tool.cron_create", "Cron");
    m.insert("tool.cron_delete", "CronDel");
    m.insert("tool.cron_list", "CronList");
    m.insert("tool.browse", "Browse");
    m.insert("tool.mcp__gitnexus__query", "GitNexus");
    m.insert("tool.mcp__gitnexus__context", "GNContext");
    m.insert("tool.mcp__gitnexus__impact", "GNImpact");
    m.insert("tool.mcp__gitnexus__cypher", "GNCypher");
    m.insert("tool.mcp__openrouter__chat", "OpenRouter");
    m.insert("tool.mcp__filesystem__read", "FSRead");
    m.insert("tool.mcp__filesystem__write", "FSWrite");
    m.insert("tool.mcp__filesystem__glob", "FSGlob");
    m.insert("tool.mcp__codex__ask", "CodexAsk");
    m.insert("tool.mcp__slack__post_message", "Slack");
    m.insert("tool.mcp__linear__create_issue", "LinearIssue");
    m.insert("tool.mcp__github__create_issue", "GHIssue");

    // Sessions detail
    m.insert("detail.session", "SESSION");
    m.insert("detail.task", "task");
    m.insert("detail.children", "CHILDREN");
    m.insert("detail.subagents", "SUBAGENTS");
    m.insert("detail.mem", "MEM");
    m.insert("detail.ctx", "CTX");
    m.insert("detail.files", "files");
    m.insert("detail.lines", "lines");
    m.insert("detail.turns", "turns");
    m.insert("detail.effort", "effort");
    m.insert("detail.timeline", "TIMELINE");
    m.insert("detail.chat", "CHAT");
    m.insert("detail.calls", "calls");
    m.insert("detail.running", "running");
    m.insert("detail.thinking", "thinking");
    m.insert("detail.generating", "generating reply");
    m.insert("detail.file_audit", "FILE AUDIT");
    m.insert("detail.accesses", "accesses");
    m.insert("detail.unique_files", "unique files");
    m.insert("detail.no_active_sessions", "no active sessions");

    // Help panel
    m.insert("help.title", " Keybindings ");
    m.insert("help.navigation", "Navigation");
    m.insert("help.actions", "Actions");
    m.insert("help.views", "Views");
    m.insert("help.help", "Help");
    m.insert("help.press_key", " Press any key to close ");
    m.insert("help.select_session", "select session");
    m.insert("help.jump_tmux", "jump to tmux pane (when in tmux)");
    m.insert("help.filter", "filter sessions");
    m.insert("help.clear_filter", "clear filter / close overlay");
    m.insert("help.kill_session", "confirm kill selected session");
    m.insert("help.kill_orphans", "confirm kill orphan ports");
    m.insert("help.refresh", "force refresh");
    m.insert("help.quit", "quit");
    m.insert("help.view_menu", "open view menu");
    m.insert("help.open_config", "open config");
    m.insert("help.cycle_theme", "cycle theme / toggle tree");
    m.insert("help.toggle_timeline", "toggle timeline");
    m.insert("help.toggle_file_audit", "toggle file audit");
    m.insert(
        "help.toggle_panels",
        "toggle panels (context/quota/tokens/projects/ports/sessions/mcp)",
    );
    m.insert(
        "help.mcp_suppress",
        "toggle mcp-server suppression in sessions panel",
    );
    m.insert("help.this_help", "this help");

    // Footer
    m.insert("footer.select", "select");
    m.insert("footer.kill", "kill");
    m.insert("footer.filter", "filter");
    m.insert("footer.workspace", "workspace");
    m.insert("footer.view", "view");
    m.insert("footer.config", "config");
    m.insert("footer.help", "help");
    m.insert("footer.quit", "quit");
    m.insert("footer.sessions", "sessions");
    m.insert("footer.auto", "auto");
    m.insert("footer.peak_hours", "Claude Peak Hours");
    m.insert("footer.resets_in", "resets in");
    m.insert("footer.esc_clear", "Esc clear, Enter keep");
    m.insert("footer.jump", "jump");

    // View menu
    m.insert("view.title", " View ");
    m.insert("view.on", "on");
    m.insert("view.off", "off");
    m.insert("view.action", "→");
    m.insert("view.tree_view", "tree view");
    m.insert("view.timeline", "timeline");
    m.insert("view.file_audit", "file audit");
    m.insert("view.context_panel", "context panel");
    m.insert("view.quota_panel", "quota panel");
    m.insert("view.tokens_panel", "tokens panel");
    m.insert("view.projects_panel", "projects panel");
    m.insert("view.ports_panel", "ports panel");
    m.insert("view.sessions_panel", "sessions panel");
    m.insert("view.mcp_servers_panel", "mcp servers panel");
    m.insert("view.mcp_session_hide", "mcp session hide");
    m.insert("view.cycle_theme", "cycle theme");
    m.insert("view.key_toggle", "key = toggle  ·  Esc = close ");

    // Header
    m.insert("header.cpu", "CPU");
    m.insert("header.mem", "MEM");
    m.insert("header.load", "L");
    m.insert("header.agents", "agents");
    m.insert("header.ctx", "ctx");

    // Tokens panel
    m.insert("tokens.total", "Total");
    m.insert("tokens.input", "Input");
    m.insert("tokens.output", "Output");
    m.insert("tokens.cache_r", "CacheR");
    m.insert("tokens.cache_w", "CacheW");
    m.insert("tokens.turns", "Turns");
    m.insert("tokens.avg", "Avg");
    m.insert("tokens.tokens_turn", "tokens/turn");

    // Context panel
    m.insert("context.rate", "Rate");
    m.insert("context.total", "Total");
    m.insert("context.active", "active");
    m.insert("context.project", "Project");
    m.insert("context.context", "Context");
    m.insert("context.window", "Window");
    m.insert("context.token_rate", "Token Rate");
    m.insert("context.no_active_sessions", "no active sessions");

    // Quota panel
    m.insert("quota.5h", "5h");
    m.insert("quota.7d", "7d");
    m.insert("quota.no_data", "no data");
    m.insert("quota.abtop_setup", "abtop --setup");
    m.insert("quota.run_codex", "run codex once");
    m.insert("quota.total", "total");
    m.insert("quota.in", "in");
    m.insert("quota.left", "left");

    // Projects panel
    m.insert("projects.no_git", "no git");
    m.insert("projects.clean", "✓clean");
    m.insert("projects.no_projects", "no projects");

    // Ports panel
    m.insert("ports.port", "PORT");
    m.insert("ports.session", "SESSION");
    m.insert("ports.orphan", "orphan");
    m.insert("ports.no_open_ports", "no open ports");
    m.insert("ports.kill_orphans", "X twice to kill orphans");

    // MCP panel
    m.insert("mcp.parent", "PARENT");
    m.insert("mcp.profile", "PROFILE");
    m.insert("mcp.act_tot", "ACT/TOT");
    m.insert("mcp.last", "LAST");
    m.insert("mcp.no_servers", "no mcp servers");
    m.insert("mcp.default", "default");
    m.insert("mcp.suppress_off", "suppress: off (M)");

    // Config panel
    m.insert("config.title", " Config ");
    m.insert("config.theme", "Theme");
    m.insert("config.on", "on");
    m.insert("config.off", "off");
    m.insert("config.change", "Enter/Space to change");
    m.insert("config.close", "Esc to close");
    m.insert("config.context_panel", "Context panel (1)");
    m.insert("config.quota_panel", "Quota panel (2)");
    m.insert("config.tokens_panel", "Tokens panel (3)");
    m.insert("config.projects_panel", "Projects panel (4)");
    m.insert("config.ports_panel", "Ports panel (5)");
    m.insert("config.sessions_panel", "Sessions panel (6)");
    m.insert("config.mcp_panel", "MCP servers (7)");

    // Terminal size too small
    m.insert("term.too_small", "Terminal size too small:");
    m.insert("term.width", "Width");
    m.insert("term.height", "Height");
    m.insert("term.needed", "Needed for current config:");

    // Time formatting
    m.insert("time.s_ago", "s ago");
    m.insert("time.m_ago", "m ago");
    m.insert("time.h_ago", "h ago");
    m.insert("time.d_ago", "d ago");
    m.insert("time.s", "s");
    m.insert("time.m", "m");
    m.insert("time.h", "h");
    m.insert("time.d", "d");

    // Misc
    m.insert("misc.dash", "—");
    m.insert("misc.active", "active");

    m
});

pub fn t(key: &str) -> String {
    LOCALE_EN
        .get(key)
        .map(|s| s.to_string())
        .unwrap_or_else(|| key.to_string())
}
