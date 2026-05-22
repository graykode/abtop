# Module: app

## Vai trò

Central state hub of abtop. Holds the entire `App` struct: live agent sessions, workspace projects, dw-kit task state, task graph, roadmap plan, controls, audit events, and tick/keypress logic. Almost every feature in the codebase reads or mutates `App`.

## Files chính

| File | Vai trò |
|------|---------|
| `src/app.rs` (~102KB) | `App` struct, `WorkspaceProject`/`WorkspaceTask` types, tick logic, key handling, summary generation, jump-to-pane, control actions, handoff/roadmap/evidence wiring |

## Public API / Exports

- `App` — central state struct
- `JumpOutcome` — enum: `Jumped` / `Failed(String)` / `NoOp` (tmux pane jump result)
- `NarrowTab` — enum: `Workspace` / `Work` / `Usage` / `System` (narrow-terminal tab modes)
- `WorkspaceProject`, `WorkspaceTask` — workspace data types consumed by `ui/`, `task_graph/`, `evidence/`, `roadmap.rs`

Constants worth knowing:
- `GRAPH_HISTORY_LEN = 200` — token-rate sparkline buffer
- `MAX_SUMMARY_JOBS = 3` — concurrent `claude --print` summary jobs
- `MAX_SUMMARY_RETRIES = 2`
- `ATTENTION_CONTEXT_WARN_PCT = 80.0` / `CRITICAL_PCT = 90.0`
- `KILL_CONFIRM_WINDOW_SECS = 2`
- `CONTROL_DRY_RUN_ENV = "ABTOP_CONTROL_DRY_RUN"`

## Dependencies

- **Upstream (depends on)**: `audit`, `collector` (MultiCollector + McpServer + read_rate_limits), `config::ControlPolicy`, `evidence`, `host_info`, `model`, `roadmap`, `task`, `task_graph`, `theme`
- **Downstream (used by)**: `main.rs` (event loop), `ui/*` (every panel reads `App`), `setup`/`doctor`/`demo` indirectly

## Conventions riêng

- Tick interval is staggered: session/transcript every 2s; ps every 2s; lsof + git + rate limits every 10s (5 ticks). This is enforced by counter logic inside `App`, not by external schedulers.
- Summary generation spawns background `claude --print` jobs with 10s timeout. Results cached to `~/.cache/abtop/summaries.json` across runs. Falls back to sanitized first prompt (28 chars) on empty/generic output. Inputs and outputs are passed through `redact_secrets` + `sanitize_terminal_text` before display.
- Mutating control actions (`kill session`, `kill orphan port`) require: explicit confirmation within `KILL_CONFIRM_WINDOW_SECS`, fresh PID-command verification, dry-run support via `ABTOP_CONTROL_DRY_RUN`, and emit an `AuditEvent`.
- Workspace project merging: same canonical project directory across multiple agents (Claude + Codex) is merged into a single `WorkspaceProject` row — see commit `784d0d8`.

## Lưu ý cho AI

- **Don't expose prompt text or file contents** through `App`-derived state. The workspace/handoff/evidence surfaces consume `App` and assume content is already redacted.
- `App` is the de-facto coupling hub between collector data and UI/export modules. Adding a field is cheap; refactoring `WorkspaceProject`/`WorkspaceTask` shape will ripple into [[task_graph]], [[evidence]], [[roadmap]], and every `ui/` panel.
- File size (~102KB) is intentional for the MVP — the team is iterating fast. Don't split it pre-emptively; wait until the shape stabilizes.
- Session status (`Working`/`Waiting`/`Error`/`Done`) is **heuristic** — see AGENTS.md "Known limitations". Don't add logic that treats it as authoritative.
- PID reuse is a real risk: verify with `ps -p {pid} -o command=` (or platform equivalent) before acting on a PID.
- Control actions must record audit events for ALL outcomes: `requested`, `confirmed`, `skipped`, `blocked`, `sent`, `failed` (per `P4-CTL-01` in EXECUTION_BOARD).
