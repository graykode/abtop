use crate::app::App;
use crate::model::{AgentSession, ChildProcess, OrphanPort, RateLimitInfo, SessionStatus, SubAgent};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn now_secs() -> u64 {
    now_ms() / 1000
}

pub fn populate_demo(app: &mut App) {
    let now = now_ms();

    // --- Sessions ---
    app.sessions = vec![
        AgentSession {
            agent_cli: "claude",
            pid: 7336,
            session_id: "a1b2c3d4-5678-9abc-def0-111111111111".into(),
            cwd: "/Users/demo/webshop".into(),
            project_name: "webshop".into(),
            started_at: now - 2 * 3600 * 1000, // 2h ago
            status: SessionStatus::Working,
            model: "claude-opus-4-6".into(),
            effort: String::new(),
            context_percent: 72.0,
            total_input_tokens: 48_200,
            total_output_tokens: 12_800,
            total_cache_read: 1_420_000,
            total_cache_create: 185_000,
            turn_count: 34,
            current_tasks: vec!["Edit src/checkout/payment.rs".into()],
            mem_mb: 342,
            version: "2.1.87".into(),
            git_branch: "main".into(),
            git_added: 2,
            git_modified: 8,
            token_history: vec![
                18000, 22000, 45000, 38000, 52000, 41000, 35000, 28000,
                61000, 55000, 48000, 39000, 44000, 50000, 32000, 27000,
                58000, 46000, 42000, 36000, 53000, 47000, 41000, 38000,
                62000, 55000, 49000, 43000, 51000, 44000, 38000, 33000,
                56000, 48000,
            ],
            subagents: vec![
                SubAgent {
                    name: "Explore test coverage".into(),

                    status: "done".into(),
                    tokens: 12_400,
                },
                SubAgent {
                    name: "Run integration tests".into(),

                    status: "working".into(),
                    tokens: 8_200,
                },
            ],
            mem_file_count: 4,
            mem_line_count: 12,
            children: vec![
                ChildProcess {
                    pid: 7401,
                    command: "cargo build --release".into(),
                    mem_kb: 342_000,
                    port: None,
                },
                ChildProcess {
                    pid: 7455,
                    command: "cargo test".into(),
                    mem_kb: 28_000,
                    port: None,
                },
            ],

            first_assistant_text: String::new(),
            initial_prompt: "Implement Stripe payment integration for checkout flow".into(),
        },
        AgentSession {
            agent_cli: "claude",
            pid: 8840,
            session_id: "b2c3d4e5-6789-abcd-ef01-222222222222".into(),
            cwd: "/Users/demo/ml-pipeline".into(),
            project_name: "ml-pipeline".into(),
            started_at: now - 47 * 60 * 1000, // 47m ago
            status: SessionStatus::Waiting,
            model: "claude-sonnet-4-6".into(),
            effort: String::new(),
            context_percent: 91.0,
            total_input_tokens: 82_000,
            total_output_tokens: 38_000,
            total_cache_read: 2_100_000,
            total_cache_create: 280_000,
            turn_count: 71,
            current_tasks: vec!["waiting for user input".into()],
            mem_mb: 128,
            version: "2.1.87".into(),
            git_branch: "feat/batch-inference".into(),
            git_added: 1,
            git_modified: 4,
            token_history: vec![
                32000, 28000, 41000, 55000, 62000, 48000, 35000, 29000,
                44000, 58000, 51000, 39000, 46000, 53000, 42000, 37000,
                60000, 52000, 45000, 38000, 56000, 49000, 43000, 36000,
                63000, 57000, 50000, 44000, 54000, 47000,
            ],
            subagents: vec![],
            mem_file_count: 2,
            mem_line_count: 8,
            children: vec![],

            first_assistant_text: String::new(),
            initial_prompt: "Add batch inference endpoint with GPU scheduling".into(),
        },
        AgentSession {
            agent_cli: "claude",
            pid: 9102,
            session_id: "c3d4e5f6-789a-bcde-f012-333333333333".into(),
            cwd: "/Users/demo/api-server".into(),
            project_name: "api-server".into(),
            started_at: now - 15 * 60 * 1000, // 15m ago
            status: SessionStatus::Working,
            model: "claude-haiku-4-5".into(),
            effort: String::new(),
            context_percent: 42.0,
            total_input_tokens: 5_200,
            total_output_tokens: 2_800,
            total_cache_read: 320_000,
            total_cache_create: 45_000,
            turn_count: 12,
            current_tasks: vec!["Bash npm run dev".into()],
            mem_mb: 86,
            version: "2.1.87".into(),
            git_branch: "main".into(),
            git_added: 0,
            git_modified: 2,
            token_history: vec![
                8000, 12000, 15000, 22000, 18000, 25000, 20000, 16000,
                28000, 24000, 19000, 14000,
            ],
            subagents: vec![],
            mem_file_count: 1,
            mem_line_count: 3,
            children: vec![
                ChildProcess {
                    pid: 9150,
                    command: "node server.js".into(),
                    mem_kb: 98_000,
                    port: Some(3000),
                },
                ChildProcess {
                    pid: 9178,
                    command: "node worker.js".into(),
                    mem_kb: 52_000,
                    port: Some(3001),
                },
                ChildProcess {
                    pid: 9203,
                    command: "postgres -D /usr/local/var/postgres".into(),
                    mem_kb: 24_000,
                    port: Some(5432),
                },
            ],

            first_assistant_text: String::new(),
            initial_prompt: "Fix CORS headers and add rate limiting middleware".into(),
        },
        AgentSession {
            agent_cli: "codex",
            pid: 8901,
            session_id: "d4e5f6a7-89ab-cdef-0123-444444444444".into(),
            cwd: "/Users/demo/data-viz".into(),
            project_name: "data-viz".into(),
            started_at: now - 5 * 60 * 1000, // 5m ago
            status: SessionStatus::Working,
            model: "gpt-5.4".into(),
            effort: "medium".into(),
            context_percent: 18.0,
            total_input_tokens: 3_100,
            total_output_tokens: 1_400,
            total_cache_read: 85_000,
            total_cache_create: 12_000,
            turn_count: 6,
            current_tasks: vec!["Write src/charts/heatmap.py".into()],
            mem_mb: 64,
            version: "0.116.0".into(),
            git_branch: "feat/heatmap".into(),
            git_added: 3,
            git_modified: 1,
            token_history: vec![
                5000, 8000, 12000, 18000, 15000, 22000,
            ],
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children: vec![
                ChildProcess {
                    pid: 8950,
                    command: "python -m http.server 8080".into(),
                    mem_kb: 32_000,
                    port: Some(8080),
                },
            ],

            first_assistant_text: String::new(),
            initial_prompt: "Create interactive heatmap component with D3.js".into(),
        },
        AgentSession {
            agent_cli: "gemini",
            pid: 6903,
            session_id: "e5f6a7b8-9abc-def0-1234-555555555555".into(),
            cwd: "/Users/demo/life-jarvis/frontend".into(),
            project_name: "frontend".into(),
            started_at: now - 25 * 60 * 1000, // 25m ago
            status: SessionStatus::Working,
            model: "gemini-3-flash-preview".into(),
            effort: String::new(),
            context_percent: 42.0,
            total_input_tokens: 25_583_957,
            total_output_tokens: 77_500,
            total_cache_read: 22_736_746,
            total_cache_create: 23_218, // thought tokens
            turn_count: 242,
            current_tasks: vec!["Deep analysis".into()],
            mem_mb: 334,
            version: "0.38.1".into(),
            git_branch: "master".into(),
            git_added: 1,
            git_modified: 0,
            token_history: vec![
                50000, 80000, 120000, 180000, 150000, 220000, 95000, 140000,
            ],
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children: vec![],
            first_assistant_text: String::new(),
            initial_prompt: "Read the file GEMINI-STRESS-TEST.md and complete all 7 tasks".into(),
        },
    ];

    // --- Summaries (pre-populated, no LLM calls) ---
    app.summaries.insert(
        "a1b2c3d4-5678-9abc-def0-111111111111".into(),
        "Stripe payment integration".into(),
    );
    app.summaries.insert(
        "b2c3d4e5-6789-abcd-ef01-222222222222".into(),
        "Batch inference endpoint".into(),
    );
    app.summaries.insert(
        "c3d4e5f6-789a-bcde-f012-333333333333".into(),
        "CORS fix + rate limiting".into(),
    );
    app.summaries.insert(
        "d4e5f6a7-89ab-cdef-0123-444444444444".into(),
        "D3 heatmap component".into(),
    );
    app.summaries.insert(
        "e5f6a7b8-9abc-def0-1234-555555555555".into(),
        "Gemini stress test tasks".into(),
    );

    // --- Rate limits ---
    app.rate_limits = vec![
        RateLimitInfo {
            source: "claude".into(),
            five_hour_pct: Some(35.0),
            five_hour_resets_at: Some(now_secs() + 3 * 3600),
            seven_day_pct: Some(12.0),
            seven_day_resets_at: Some(now_secs() + 5 * 24 * 3600),
            updated_at: Some(now_secs() - 10),
        },
        RateLimitInfo {
            source: "codex".into(),
            five_hour_pct: Some(9.0),
            five_hour_resets_at: Some(now_secs() + 4 * 3600),
            seven_day_pct: Some(14.0),
            seven_day_resets_at: Some(now_secs() + 6 * 24 * 3600),
            updated_at: Some(now_secs() - 5),
        },
    ];

    // --- Token rates (synthetic sparkline) ---
    let rates = [
        0.0, 0.0, 120.0, 340.0, 580.0, 420.0, 0.0, 0.0, 890.0, 1200.0,
        950.0, 680.0, 0.0, 0.0, 450.0, 780.0, 1100.0, 1350.0, 920.0, 610.0,
        0.0, 340.0, 670.0, 890.0, 1050.0, 780.0, 520.0, 0.0, 0.0, 1400.0,
        1180.0, 850.0, 620.0, 410.0, 0.0, 560.0, 820.0, 1060.0, 1280.0, 940.0,
        700.0, 480.0, 0.0, 0.0, 380.0, 720.0, 1150.0, 1320.0, 980.0, 650.0,
    ];
    app.token_rates.clear();
    for &r in &rates {
        app.token_rates.push_back(r);
    }

    // --- Orphan ports ---
    app.orphan_ports = vec![
        OrphanPort {
            port: 4000,
            pid: 6543,
            command: "node dist/server.js".into(),
            project_name: "old-project".into(),
        },
    ];
}
