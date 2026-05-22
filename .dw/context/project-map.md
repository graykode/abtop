# Project Map: abtop

## Ngày tạo: 2026-05-17
## Tạo bởi: dw-onboard

---

## Tech Stack

- **Ngôn ngữ**: Rust (edition 2021, rust-version 1.88)
- **Framework**: ratatui 0.29 + crossterm 0.28 (TUI)
- **Serialization**: serde + serde_json (JSON/JSONL)
- **Time**: chrono 0.4
- **Filesystem**: dirs 6, tempfile 3
- **Platform-specific**:
  - macOS: `proc_pidinfo`
  - Windows: `sysinfo`
  - Linux: `libc`
- **Build/Distribution**: cargo + `cargo-dist` (`dist-workspace.toml`)
- **CI/Release**: GitHub Actions — release.yml (binaries + Homebrew) + publish.yml (crates.io)
- **No runtime services**: read-only over local filesystem + `ps`/`lsof`/`netstat`. No API calls, no auth.

## Cấu Trúc Tổng Quan

```
abtop/
├── AGENTS.md             # Canonical architecture + data sources + privacy rules (READ FIRST)
├── CLAUDE.md             # Pointer to AGENTS.md
├── Cargo.toml            # v0.4.4
├── README.md
├── docs/                 # Product strategy, roadmap, agent handoff, execution board
│   ├── AGENT_HANDOFF.md
│   ├── EXECUTION_BOARD.md      # Single source of truth for in-flight task status
│   ├── PRODUCT_STRATEGY.md
│   ├── ROADMAP_V2.md
│   └── ...
├── scripts/
│   └── abtop-statusline.sh     # StatusLine hook installed by `abtop --setup`
├── src/
│   ├── main.rs           # Entry, CLI flag dispatch, terminal setup, event loop
│   ├── app.rs            # Central App state (102KB) — workspace model, tick logic
│   ├── config.rs         # AppConfig, ControlPolicy, PanelVisibility, hidden_agents
│   ├── demo.rs           # Demo data fixture for --demo flag
│   ├── diagnostics.rs    # Logging (log_info!/log_debug! macros)
│   ├── doctor.rs         # `--doctor` self-check (collectors + setup status)
│   ├── host_info.rs      # Host CPU/RAM sampling
│   ├── locale.rs         # i18n (English only currently)
│   ├── roadmap.rs        # Dependency-aware roadmap planning + export
│   ├── setup.rs          # `--setup` installs StatusLine hook (Windows PowerShell + sh)
│   ├── theme.rs          # Color themes (btop default + others)
│   ├── audit/            # Append-only audit log for mutating control actions
│   ├── collector/        # claude/codex/opencode session discovery + transcript parsing
│   ├── evidence/         # Per-task safe evidence bundle export
│   ├── model/            # AgentSession, OrphanPort, RateLimitInfo, SessionStatus
│   ├── task/             # dw-kit task index reader (.dw/tasks parser)
│   ├── task_graph/       # Workspace task graph (nodes/edges) for visual surfaces
│   └── ui/               # All TUI panels: workspace, sessions, quota, tokens, ports, etc.
├── assets/               # Demo GIFs (excluded from crates.io publish)
├── target/               # Build output (gitignored)
├── .dw/                  # dw-kit workspace state (this onboarding)
│   ├── config/
│   ├── context/          # ← project-map.md + modules/ live here
│   ├── decisions/
│   ├── tasks/            # ACTIVE.md + per-task spec.md/tracking.md
│   └── ...
└── .github/              # CI workflows
```

## Modules

Complexity legend: **Cao** = >10 files OR >40KB OR core business logic OR cross-cutting; **TB** = focused subsystem; **Thấp** = thin utility.

| Module | Type | Vai trò | Phức tạp | Active? | Deep-dive? |
|--------|------|---------|----------|---------|------------|
| `src/main.rs` | entry | CLI flag routing (`--setup`/`--doctor`/`--demo`/`--workspace-summary`/`--roadmap`/`--handoff`/`--task-evidence`), terminal init, event loop | TB | Có | — |
| `src/app.rs` | core | Central `App` state: sessions, workspace projects, tasks, graph, roadmap, controls, handoff. Tick logic + key handling | **Cao** | Có | `/dw:retroactive app` |
| `src/collector/` | infra | Session discovery + transcript parsing for Claude Code, Codex CLI, OpenCode + process/ports/rate-limit | **Cao** | Có | `/dw:retroactive collector` |
| `src/ui/` | feature | All ratatui panels (workspace, sessions, quota, tokens, ports, context, footer, header, help, view_menu, config, projects, mcp) | **Cao** | Có | `/dw:retroactive ui` |
| `src/task/` | feature | dw-kit task index parser (`.dw/tasks/**`) → `DwTaskSummary`, `TaskStatus`. Core moat | **Cao** | Có | `/dw:retroactive task` |
| `src/task_graph/` | feature | Graph nodes/edges (Project, Task, Decision, Agent, Risk) — data model for visual surfaces | **Cao** | Có | `/dw:retroactive task_graph` |
| `src/evidence/` | feature | Safe per-task evidence bundles (counts, agents, tools, files, risks) + Markdown render | **Cao** | Có | `/dw:retroactive evidence` |
| `src/roadmap.rs` | feature | Dependency-aware roadmap planning (Ready/Blocked stages, risks) | **Cao** | Có | `/dw:retroactive roadmap` |
| `src/audit/` | infra | Append-only audit log for mutating control actions | TB | Có | — |
| `src/model/` | infra | Shared data types: `AgentSession`, `OrphanPort`, `RateLimitInfo`, `SessionStatus`, `FileAccess`, `ToolCall` | TB | Stable | — |
| `src/config.rs` | infra | `AppConfig`, `ControlPolicy`, `PanelVisibility`, hidden_agents | TB | Có | — |
| `src/setup.rs` | infra | `abtop --setup`: install StatusLine hook (PowerShell on Windows, sh elsewhere) | TB | Stable | — |
| `src/doctor.rs` | infra | `--doctor`: validate collectors + setup config | TB | Có | — |
| `src/demo.rs` | infra | Synthetic data for `--demo` flag (used in CI/screenshots) | TB | Có | — |
| `src/theme.rs` | infra | Color themes (btop default) | TB | Stable | — |
| `src/locale.rs` | infra | i18n strings (English currently) | TB | Stable | — |
| `src/host_info.rs` | infra | Host CPU/RAM sampling | Thấp | Stable | — |
| `src/diagnostics.rs` | infra | Opt-in logging macros | Thấp | Stable | — |

## Dependencies giữa Modules

```
main.rs
  └─→ app.rs (App, JumpOutcome)
        ├─→ collector/ (MultiCollector, ClaudeCollector, CodexCollector, OpenCodeCollector, read_rate_limits, redact_secrets)
        │     └─→ model/ (AgentSession, FileAccess, ToolCall, RateLimitInfo)
        ├─→ task/ (read_project_state, DwTaskSummary, TaskStatus)
        ├─→ task_graph/ (TaskGraph, GraphNodeKind)   ← depends on app types (WorkspaceProject, WorkspaceTask)
        ├─→ roadmap.rs (build_project_roadmap, RoadmapPlan, RoadmapRisk)
        ├─→ evidence/ (build_task_evidence, render_task_evidence_markdown)  ← uses task_graph + model
        ├─→ audit/ (AuditEvent, record)
        ├─→ host_info.rs (HostMetrics, HostSampler, AgentAggregate)
        ├─→ config.rs (ControlPolicy)
        └─→ model/, theme.rs

ui/ (all panels)
  └─→ app.rs (App, WorkspaceProject, WorkspaceTask) + theme + model

main.rs also wires:
  ├─→ setup.rs    (--setup)
  ├─→ doctor.rs   (--doctor)
  ├─→ demo.rs     (--demo)
  ├─→ diagnostics.rs (logging)
  └─→ locale.rs   (UI strings)
```

Coupling note: `task_graph/`, `evidence/`, and `ui/` depend on `app::{WorkspaceProject, WorkspaceTask}` types. `app.rs` is the de-facto hub — be careful when refactoring its public surface.

## Entry Points chính

- `abtop` (no args) — interactive TUI (ratatui alt-screen, raw mode, mouse capture)
- `abtop --once` — print snapshot and exit (redacts tool inputs)
- `abtop --setup` — install StatusLine hook for Claude rate limits
- `abtop --doctor [--json]` — collector + setup diagnostics
- `abtop --demo` — synthetic fixture data
- `abtop --workspace-summary` — Markdown export of workspace + task state (redacted)
- `abtop --roadmap` — dependency-aware roadmap export (Markdown)
- `abtop --handoff [--json]` — cross-agent handoff plan (Markdown or JSON)
- `abtop --task-evidence` — per-task evidence bundles (Markdown)
- `abtop --exit-on-jump` — quit after Enter-jumping into a tmux pane
- `abtop --update` — self-update via GitHub releases installer
- `abtop --version` / `--help`

## Conventions phát hiện

- **English-only**: source, comments, tests, docs, GitHub artifacts. Enforced by `AGENTS.md` Language Policy. Non-English text only for external identifiers or quoted input.
- **Commit format**: `<type>: <description>` — types `feat|fix|refactor|docs|chore` (note: AGENTS.md says NO `Co-authored-by` or AI attribution trailers; this differs from `.claude/rules/commit-standards.md` which suggests `Co-Authored-By: Claude` — repo convention wins).
- **Branch**: long-lived work happens on `codex/agentic-workspace-mvp`; `main` tracks upstream.
- **Defensive parsing**: `serde(default)` everywhere — all data sources are undocumented Claude Code/Codex internals.
- **Privacy by default**: never display prompt text or file contents in workspace surfaces; redact tool inputs in `--once`; safe exports use titles/counts/statuses only.
- **Polling intervals** (staggered to avoid freezes): session/transcript every 2s; ps every 2s; lsof + git + rate limits every 10s (5 ticks).
- **Heuristic status**: session status (Working/Waiting/Error/Done) is best-effort, not authoritative — documented in AGENTS.md "Gotchas".
- **Per-platform code paths** via `cfg(target_os = ...)` for process inspection (proc_pidinfo / sysinfo / libc).

## Git Activity (3 tháng gần nhất)

- **Active modules**: `task/`, `task_graph/`, `evidence/`, `audit/`, `app.rs`, `ui/workspace.rs`, `roadmap.rs` (the Agentic Workspace MVP push)
- **Stable modules**: `model/`, `theme.rs`, `host_info.rs`, `diagnostics.rs`, `scripts/`, basic UI panels
- **Top contributors (3 months)**: graykode (232) — upstream maintainer; Tae Hwan Jung (87) — upstream; Thomas BOUQUET-GASPAROUX (48); **huydv (39) — fork owner driving the Agentic Workspace work**; KorenKrita, Elon Demirok, ybb, k-wilkinson, AntaresGG, xiaoye5200

Recent fork commits (codex/agentic-workspace-mvp):
- `784d0d8` fix: merge same-project agent handoffs
- `bb59f4b` feat: add production handoff surface
- `9856556` feat: add structured handoff export
- `92227bb` feat: add cross-agent handoff export
- `350a58f` feat: add roadmap export and control policy gates

## Current Initiative

**Agentic Workspace MVP** (branch `codex/agentic-workspace-mvp`) — turning abtop from a btop-style monitor into a local-first Agentic Workspace: flight recorder + operations cockpit + task viewer + safety layer for multi-agent software work.

Moat formula (per `docs/AGENT_HANDOFF.md`):
```
dw-kit task graph + abtop runtime graph = agentic work graph
```

Current next task: **`P5-GTM-05`** — release packaging and user-facing onboarding from validated local baseline (per `docs/EXECUTION_BOARD.md`).

## Gợi ý Deep-dive

Modules phức tạp Cao nên chạy `/dw:retroactive` để AI có context đầy đủ trước khi sửa lớn:

- [ ] `/dw:retroactive app` — central hub (102KB), tick logic + workspace model, touched by almost every feature task
- [ ] `/dw:retroactive collector` — 7 files, ~300KB, parses undocumented Claude/Codex/OpenCode internals
- [ ] `/dw:retroactive task` — dw-kit parser, core moat surface
- [ ] `/dw:retroactive task_graph` — graph data model feeding roadmap/evidence/UI
- [ ] `/dw:retroactive evidence` — safe export pipeline, privacy-critical
- [ ] `/dw:retroactive roadmap` — dependency-aware planning logic
- [ ] `/dw:retroactive ui` — 14 panels, layout priority, visible workspace surface

## Required Reading Order for New Agents

Per `docs/AGENT_HANDOFF.md` "First Five Minutes":

1. `AGENTS.md` — architecture, data sources, privacy rules, gotchas (canonical)
2. `docs/EXECUTION_BOARD.md` — claim a `Next` task, set to `Doing` before editing
3. `docs/ROADMAP_V2.md` — milestone context (P0..P5)
4. `git status --short --branch` + `git log --oneline -5`
5. Product direction questions → `docs/PRODUCT_STRATEGY.md` + `docs/COMPETITIVE_MAP.md`

For shorter resume: `docs/COMPACT_CONTEXT.md`.
