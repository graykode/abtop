// i18n module
use std::env;
use std::sync::LazyLock;

static CURRENT_LANG: LazyLock<&str> = LazyLock::new(|| {
    let from_config = crate::config::load_config().language;
    let lang = if !from_config.is_empty() {
        from_config
    } else {
        env::var("LANG").unwrap_or_default()
    };
    if lang.to_lowercase().starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
});

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
    m.insert("help.kill_session", "kill selected session");
    m.insert("help.kill_orphans", "kill orphan ports");
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

    // Projects panel
    m.insert("projects.no_git", "no git");
    m.insert("projects.clean", "✓clean");
    m.insert("projects.no_projects", "no projects");

    // Ports panel
    m.insert("ports.port", "PORT");
    m.insert("ports.session", "SESSION");
    m.insert("ports.orphan", "orphan");
    m.insert("ports.no_open_ports", "no open ports");
    m.insert("ports.kill_orphans", "X to kill orphans");

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

static LOCALE_ZH: LazyLock<std::collections::HashMap<&str, &str>> = LazyLock::new(|| {
    let mut m = std::collections::HashMap::new();

    // Status icons
    m.insert("sess.think", "◉ 思考");
    m.insert("sess.exec", "● 执行");
    m.insert("sess.wait", "◌ 等待");
    m.insert("sess.rate", "⏳ 限速");
    m.insert("sess.done", "✓ 完成");

    // Column headers
    m.insert("col.ai", "AI");
    m.insert("col.pid", "PID");
    m.insert("col.project", "项目");
    m.insert("col.session", "会话");
    m.insert("col.sess", "会");
    m.insert("col.summary", "摘要");
    m.insert("col.status", "状态");
    m.insert("col.model", "模型");
    m.insert("col.context", "上下文");
    m.insert("col.ctx", "上");
    m.insert("col.tokens", "Token");
    m.insert("col.memory", "内存");
    m.insert("col.turn", "轮");

    // Agent labels
    m.insert("agent.claude", "*CC");
    m.insert("agent.codex", ">CD");

    // Tool labels
    m.insert("tool.bash", "终端");
    m.insert("tool.read", "读取");
    m.insert("tool.write", "写入");
    m.insert("tool.edit", "编辑");
    m.insert("tool.glob", "匹配");
    m.insert("tool.grep", "搜索");
    m.insert("tool.brexec", "批执行");
    m.insert("tool.web_search", "搜索");
    m.insert("tool.web_fetch", "抓取");
    m.insert("tool.tdd", "TDD");
    m.insert("tool.investigate", "调查");
    m.insert("tool.lsp", "LSP");
    m.insert("tool.notebook_edit", "笔记本");
    m.insert("tool.task_create", "建任务");
    m.insert("tool.task_update", "更任务");
    m.insert("tool.task_list", "列任务");
    m.insert("tool.task_get", "查任务");
    m.insert("tool.cron_create", "定时");
    m.insert("tool.cron_delete", "删定时");
    m.insert("tool.cron_list", "列定时");
    m.insert("tool.browse", "浏览");
    m.insert("tool.mcp__gitnexus__query", "GN查询");
    m.insert("tool.mcp__gitnexus__context", "GN上下文");
    m.insert("tool.mcp__gitnexus__impact", "GN影响");
    m.insert("tool.mcp__gitnexus__cypher", "GN查询");
    m.insert("tool.mcp__openrouter__chat", "路由");
    m.insert("tool.mcp__filesystem__read", "FS读");
    m.insert("tool.mcp__filesystem__write", "FS写");
    m.insert("tool.mcp__filesystem__glob", "FS匹配");
    m.insert("tool.mcp__codex__ask", "Codex问");
    m.insert("tool.mcp__slack__post_message", "Slack");
    m.insert("tool.mcp__linear__create_issue", "Linear");
    m.insert("tool.mcp__github__create_issue", "GH问题");

    // Sessions detail
    m.insert("detail.session", "会话");
    m.insert("detail.task", "任务");
    m.insert("detail.children", "子进程");
    m.insert("detail.subagents", "子代理");
    m.insert("detail.mem", "内存");
    m.insert("detail.ctx", "上下文");
    m.insert("detail.files", "文件");
    m.insert("detail.lines", "行");
    m.insert("detail.turns", "轮");
    m.insert("detail.effort", "投入");
    m.insert("detail.timeline", "时间线");
    m.insert("detail.calls", "调用");
    m.insert("detail.running", "运行中");
    m.insert("detail.thinking", "思考中");
    m.insert("detail.generating", "生成回复");
    m.insert("detail.file_audit", "文件审计");
    m.insert("detail.accesses", "访问");
    m.insert("detail.unique_files", "唯一文件");
    m.insert("detail.no_active_sessions", "无活跃会话");

    // Help panel
    m.insert("help.title", " 快捷键 ");
    m.insert("help.navigation", "导航");
    m.insert("help.actions", "操作");
    m.insert("help.views", "视图");
    m.insert("help.help", "帮助");
    m.insert("help.press_key", " 按任意键关闭 ");
    m.insert("help.select_session", "选择会话");
    m.insert("help.jump_tmux", "跳转到 tmux 面板");
    m.insert("help.filter", "过滤会话");
    m.insert("help.clear_filter", "清除过滤 / 关闭覆盖");
    m.insert("help.kill_session", "终止选中的会话");
    m.insert("help.kill_orphans", "终止孤立端口");
    m.insert("help.refresh", "强制刷新");
    m.insert("help.quit", "退出");
    m.insert("help.view_menu", "打开视图菜单");
    m.insert("help.open_config", "打开配置");
    m.insert("help.cycle_theme", "切换主题 / 切换树视图");
    m.insert("help.toggle_timeline", "切换时间线");
    m.insert("help.toggle_file_audit", "切换文件审计");
    m.insert(
        "help.toggle_panels",
        "切换面板 (上下文/配额/令牌/项目/端口/会话/MCP)",
    );
    m.insert("help.mcp_suppress", "切换会话面板中的 MCP 服务器隐藏");
    m.insert("help.this_help", "显示帮助");

    // Footer
    m.insert("footer.select", "选择");
    m.insert("footer.kill", "终止");
    m.insert("footer.filter", "过滤");
    m.insert("footer.view", "视图");
    m.insert("footer.config", "配置");
    m.insert("footer.help", "帮助");
    m.insert("footer.quit", "退出");
    m.insert("footer.sessions", "会话");
    m.insert("footer.auto", "自动");
    m.insert("footer.peak_hours", "Claude 高峰时段");
    m.insert("footer.resets_in", "重置于");
    m.insert("footer.esc_clear", "Esc 清除，Enter 保留");
    m.insert("footer.jump", "跳转");

    // View menu
    m.insert("view.title", " 视图 ");
    m.insert("view.on", "开");
    m.insert("view.off", "关");
    m.insert("view.action", "→");
    m.insert("view.tree_view", "树视图");
    m.insert("view.timeline", "时间线");
    m.insert("view.file_audit", "文件审计");
    m.insert("view.context_panel", "上下文面板");
    m.insert("view.quota_panel", "配额面板");
    m.insert("view.tokens_panel", "令牌面板");
    m.insert("view.projects_panel", "项目面板");
    m.insert("view.ports_panel", "端口面板");
    m.insert("view.sessions_panel", "会话面板");
    m.insert("view.mcp_servers_panel", "MCP 服务器面板");
    m.insert("view.mcp_session_hide", "隐藏 MCP 会话");
    m.insert("view.cycle_theme", "切换主题");
    m.insert("view.key_toggle", "按键切换  ·  Esc 关闭 ");

    // Header
    m.insert("header.cpu", "CPU");
    m.insert("header.mem", "内存");
    m.insert("header.load", "负载");
    m.insert("header.agents", "代理");
    m.insert("header.ctx", "上下文");

    // Tokens panel
    m.insert("tokens.total", "总计");
    m.insert("tokens.input", "输入");
    m.insert("tokens.output", "输出");
    m.insert("tokens.cache_r", "缓存读");
    m.insert("tokens.cache_w", "缓存写");
    m.insert("tokens.turns", "轮数");
    m.insert("tokens.avg", "平均");
    m.insert("tokens.tokens_turn", "令牌/轮");

    // Context panel
    m.insert("context.rate", "速率");
    m.insert("context.total", "总计");
    m.insert("context.active", "活跃");
    m.insert("context.project", "项目");
    m.insert("context.context", "上下文");
    m.insert("context.window", "窗口");
    m.insert("context.token_rate", "Token 速率");
    m.insert("context.no_active_sessions", "无活跃会话");

    // Quota panel
    m.insert("quota.5h", "5小时");
    m.insert("quota.7d", "7天");
    m.insert("quota.no_data", "无数据");
    m.insert("quota.abtop_setup", "abtop --setup");
    m.insert("quota.run_codex", "运行一次 codex");
    m.insert("quota.total", "总计");
    m.insert("quota.in", "还有");

    // Projects panel
    m.insert("projects.no_git", "非 Git");
    m.insert("projects.clean", "✓干净");
    m.insert("projects.no_projects", "无项目");

    // Ports panel
    m.insert("ports.port", "端口");
    m.insert("ports.session", "会话");
    m.insert("ports.orphan", "孤立");
    m.insert("ports.no_open_ports", "无开放端口");
    m.insert("ports.kill_orphans", "X 终止孤立");

    // MCP panel
    m.insert("mcp.parent", "父进程");
    m.insert("mcp.profile", "配置");
    m.insert("mcp.act_tot", "活跃/总计");
    m.insert("mcp.last", "最近");
    m.insert("mcp.no_servers", "无 MCP 服务器");
    m.insert("mcp.default", "默认");
    m.insert("mcp.suppress_off", "隐藏: 关闭 (M)");

    // Config panel
    m.insert("config.title", " 配置 ");
    m.insert("config.theme", "主题");
    m.insert("config.on", "开");
    m.insert("config.off", "关");
    m.insert("config.change", "Enter/空格 更改");
    m.insert("config.close", "Esc 关闭");
    m.insert("config.context_panel", "上下文面板 (1)");
    m.insert("config.quota_panel", "配额面板 (2)");
    m.insert("config.tokens_panel", "令牌面板 (3)");
    m.insert("config.projects_panel", "项目面板 (4)");
    m.insert("config.ports_panel", "端口面板 (5)");
    m.insert("config.sessions_panel", "会话面板 (6)");
    m.insert("config.mcp_panel", "MCP 服务器 (7)");

    // Terminal size too small
    m.insert("term.too_small", "终端尺寸过小:");
    m.insert("term.width", "宽度");
    m.insert("term.height", "高度");
    m.insert("term.needed", "当前配置需要:");

    // Time formatting
    m.insert("time.s_ago", "秒前");
    m.insert("time.m_ago", "分前");
    m.insert("time.h_ago", "时前");
    m.insert("time.d_ago", "天前");
    m.insert("time.s", "秒");
    m.insert("time.m", "分");
    m.insert("time.h", "时");
    m.insert("time.d", "天");

    // Misc
    m.insert("misc.dash", "—");
    m.insert("misc.active", "活跃");

    m
});

pub fn t(key: &str) -> String {
    if *CURRENT_LANG == "zh-CN" {
        LOCALE_ZH
            .get(key)
            .map(|s| s.to_string())
            .unwrap_or_else(|| key.to_string())
    } else {
        LOCALE_EN
            .get(key)
            .map(|s| s.to_string())
            .unwrap_or_else(|| key.to_string())
    }
}
