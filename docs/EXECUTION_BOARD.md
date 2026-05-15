# Execution Board

This is the operational task board for humans and agents working on this fork.
Use it together with `docs/PRODUCT_STRATEGY.md`, `docs/ROADMAP_V2.md`, and
`docs/AGENT_HANDOFF.md`.

## Status Values

- `Done`: implemented, verified, committed, and pushed.
- `Doing`: currently owned by an active agent or human.
- `Next`: ready to start.
- `Backlog`: shaped, but not ready to start.
- `Blocked`: waiting on product decision, dependency, or external validation.

## Owner Rules

- Before editing files, claim one `Next` task by changing it to `Doing`.
- Do not work on a `Doing` task owned by another agent unless the user asks.
- Keep write sets narrow and documented.
- Update EVD before marking a task `Done`.
- If work is interrupted, leave a short handoff note in this board.

## Current Focus

`P2-VIS-02`: prototype the mind-map data model after the read-only TUI task
tree has landed.

## Task Board

| ID | Status | Owner | Track | Task | Outcome | Dependencies | Write Scope | EVD |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| P0-WIN-01 | Done | Codex | Windows baseline | Native Windows StatusLine setup | `abtop --setup` installs PowerShell hook and Claude quota works on Windows. | None | `src/setup.rs`, docs | `cargo test setup`; `rate_limits=2`; commits `c8177ad`, `a66f430` |
| P0-WIN-02 | Done | Codex | Windows baseline | Clarify quota semantics | Quota panel labels rate-limit remaining and docs explain total tokens vs remaining percent. | P0-WIN-01 | `src/ui/quota.rs`, docs | `desktop_quota_labels_remaining_percent`; commit `f5617c0` |
| P0-WIN-03 | Done | Codex | Windows baseline | Windows TCP port parsing | `netstat -ano -p TCP` parsing handles IPv4, IPv6, duplicate rows, and non-listening rows. | None | `src/collector/process.rs` | `parse_windows_netstat_ports_*`; commit `d10f5aa` |
| P0-UP-01 | Done | Codex | Fork hygiene | Upstream sync guide | Fork has repeatable upstream merge/cherry-pick/conflict workflow. | None | `docs/UPSTREAM_SYNC.md` | `git fetch upstream`; commit `cbaa87e` |
| P0-UP-02 | Done | Codex | Fork hygiene | Sync upstream OpenCode fix | macOS OpenCode cwd lookup uses `lsof -a` upstream fix. | P0-UP-01 | `src/collector/opencode.rs` | `cargo test opencode`; commit `c8a3803` |
| P1-T01 | Done | Codex + Peirce | Task-aware workspace | dw-kit task index reader | Parse dw-kit task/project metadata into a safe internal model. | Product strategy docs | `src/task/*`, `src/app.rs`, tests | `cargo fmt -- --check`; `cargo test task`; `cargo test workspace`; `cargo test`; `cargo clippy --all-targets --all-features -- -D warnings`; demo summary |
| P1-T02 | Done | Codex + Beauvoir | Task-aware workspace | Workspace task detail pane v2 | Show active task, phase, acceptance criteria count, decisions, verification status, and next action. | P1-T01 | `src/app.rs`, `src/ui/workspace.rs`, task model | `desktop_workspace_focus_renders_dw_task_lens`; `cargo test workspace` |
| P1-T03 | Done | Codex | Task-aware workspace | Safe task snapshot export | Extend `--workspace-summary` with task state without prompt/file contents. | P1-T01 | `src/app.rs`, tests, docs | `workspace_summary_markdown_is_redacted_and_structured`; demo summary output includes workflow counts |
| P1-T04 | Done | Peirce + Codex | Task-aware workspace | Task status normalization | Map dw-kit state to `ready`, `doing`, `blocked`, `review`, `done`, and `unknown`. | P1-T01 | task model, tests | `task::dw::tests::*`; app next-action mapping |
| P2-VIS-01 | Done | Codex | Visual task viewer | TUI task tree view | Add read-only task tree before any graphical mind map. | P1-T01, P1-T02 | UI module, tests | `cargo fmt -- --check`; `cargo test workspace`; `cargo test`; `cargo clippy --all-targets --all-features -- -D warnings`; refreshed `assets/workspace-demo.gif` |
| P2-VIS-02 | Next | Unassigned | Visual task viewer | Mind-map data model prototype | Create graph nodes/edges for tasks, decisions, sessions, files, and risks. | P2-VIS-01 | `src/task_graph/*`, docs | Pending |
| P3-EVD-01 | Backlog | Unassigned | Evidence bundles | Per-task evidence bundle | Export safe per-task evidence: sessions, commands, files touched, checks, decisions. | P1-T03 | export module, tests | Pending |
| P4-AUD-01 | Blocked | Unassigned | Controls | Local audit log | Add append-only audit log before any mutating control action. | Product decision | audit module, docs | Pending |
| P4-CTL-01 | Blocked | Unassigned | Controls | Mutating control actions | Kill/restart/archive/dispatch actions with confirmation and audit. | P4-AUD-01 | app/ui/control modules | Pending |

## Next Task Detail: P2-VIS-02

Target user:

- Solo power user and small team that wants to understand how tasks, agents,
  decisions, files, and risks relate across projects.

Pain solved:

- The TUI task tree is useful for scanning task state, but it does not yet model
  cross-object relationships for a future mind-map or graph lens.

Hypothesis:

- A separate graph data model lets us experiment with mind-map UX without
  entangling it with terminal rendering.

Data sources:

- `WorkspaceProject` task rollups,
- `WorkspaceTask` summaries,
- session summaries, tool calls, decisions, records, and touched files where
  safe to expose.

Privacy risk:

- Relationship graphs can leak sensitive context through labels. Use only
  sanitized task titles, counts, relative identifiers, and redacted relationship
  types.

Expected design:

- Add a pure data model first: nodes, edges, node kinds, and redacted labels.
- Keep rendering separate from graph construction.
- Start with tests over demo data and `.dw` fixtures before adding UI.

Suggested write scope:

- `src/task_graph/*`,
- `src/app.rs` only if a bridge is needed,
- tests for graph construction and privacy boundaries.

EVD target:

- `cargo test task_graph`,
- `cargo test workspace`,
- `cargo fmt -- --check`,
- `cargo clippy --all-targets --all-features -- -D warnings`.

## Handoff Notes

- Keep `AW-014` blocked until audit and confirmation UX exist.
- Avoid large refactors in `src/app.rs`; prefer extracting new task/workspace
  modules.
- Do not commit local EVD files that include private paths, quota, prompts, or
  screenshots.
- Current branch: `codex/agentic-workspace-mvp`.
