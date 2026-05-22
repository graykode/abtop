# Module: ui

## Vai trò

All ratatui rendering. 14 panels covering the Workspace view, Sessions table + detail, Quota gauges, Tokens panel, Ports/MCP panels, Context sparkline, Footer/Header, Help overlay, View menu, Config panel, Projects panel. Reads from `App` and `theme`; never mutates state.

## Files chính

| File | Vai trò |
|------|---------|
| `mod.rs` (~55KB) | Layout orchestration (priority allocation: sessions → mid-tier → context → header/footer), shared helpers (`btop_block_active`, `fmt_tokens`, `grad_at`, `make_gradient`, `truncate_str`) |
| `sessions.rs` (~51KB) | Session list table + selected session detail (children, subagents, memory status, version) |
| `workspace.rs` (~26KB) | Workspace tab: agentic surface, dw-task lens, compact handoff lanes, assignment suggestions |
| `quota.rs` (~10KB) | Claude + Codex 5h/7d rate-limit gauges with reset countdown |
| `context.rs` (~8KB) | Token-rate braille sparkline (200pt history) + per-session context % bars |
| `tokens.rs` (~6KB) | Per-session token breakdown (input/output/cache) + per-turn sparkline |
| `footer.rs` (~7KB) | Keybindings + status messages |
| `header.rs` (~4KB) | Title bar |
| `help.rs` (~3KB) | Help overlay |
| `ports.rs` (~4KB) | Agent + orphan ports list |
| `projects.rs` (~3KB) | Per-project git status |
| `mcp.rs` (~4KB) | MCP server list |
| `config.rs` (~4KB) | Config overlay |
| `view_menu.rs` (~5KB) | View toggle menu |

## Public API / Exports

- Frame-drawing functions called by `main.rs` event loop (per-panel `draw_*` entry points)
- Shared layout helpers in `mod.rs` (re-used across panels)

## Dependencies

- **Upstream**: `app` (every panel reads `App`), `theme`, `model`, `task`, `task_graph`, `roadmap`
- **Downstream**: `main.rs` (terminal init + tick render loop)
- **External**: `ratatui::{Frame, prelude::*}`, `crossterm` events (only for input mapping — actual handling is in `app`)

## Conventions riêng

- **Panel rendering priority** (top to bottom, documented in AGENTS.md):
  1. Sessions — always visible, min 5 rows, ideal 2/session + 7
  2. Mid-tier (quota, tokens, projects, ports) — split equally, shown if space allows
  3. Context — only if sessions have ideal height AND surplus ≥ 5 rows
  4. Header (1 row) + Footer (1 row) — always present
- **Narrow terminal mode**: switches to 4 stacked tabs (`Workspace | Work | Usage | System`) via `NarrowTab` in `app`.
- **Privacy**: tool inputs render as `tool_name first_arg` only — never file contents or prompt text. This is enforced at the rendering layer, but collectors already redact upstream as defense-in-depth.
- **Minimum terminal size 80x24** — degrade gracefully (context panel hidden first).
- **No data collection here** — UI is purely a view layer. If a panel needs a derived value, compute it in `app` and expose it as a field.

## Lưu ý cho AI

- The workspace view (`workspace.rs`) is the visible face of the moat. Test `desktop_workspace_focus_renders_dw_task_lens` covers the dw-task lens; touch with care.
- `sessions.rs` and `mod.rs` are both ~50KB — historical decision to keep them as single files. Don't split pre-emptively.
- Themes: btop default + others in `theme.rs`. `--theme` CLI flag overrides config-file theme.
- Recent commits: `7e00f95 feat: activate sessions from workspace`, `ec20099 feat: show selected workspace sessions`, `003883a feat: add workspace project selection`, `e79d9f5 feat: prioritize workspace attention signals`, `e738c4f feat: surface workspace task state` — the Workspace tab is where active fork work is concentrated.
- Quota panel: Claude + Codex only. OpenCode does NOT expose account-level rate limits — don't add a row.
- Demo GIF regeneration: only when the visible TUI flow changes (per `docs/AGENT_HANDOFF.md`).
