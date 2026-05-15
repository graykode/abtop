# Agentic Workspace Tracker

This tracker is the working task board for the Agentic Workspace effort. Keep
it updated whenever a slice lands so the fork has a clear record of what is
done, what is active, and what is next.

Status values:

- `Done`: implemented, tested, and pushed.
- `Doing`: current implementation focus.
- `Next`: ready to pick up after the current slice.
- `Backlog`: planned but not yet shaped.
- `Blocked`: waiting on external data, product choice, or dependency.

## Current Focus

`AW-012`: add a selected-project timeline strip.

## Task Board

| ID | Status | Task | Outcome | EVD |
| --- | --- | --- | --- | --- |
| AW-001 | Done | Read-only Workspace entry point | `a` opens Agentic Workspace in compact and desktop layouts. | `cargo test workspace`; commit `1993954` |
| AW-002 | Done | Desktop Workspace shortcut fix | `a` visibly opens Workspace focus in wide terminals. | `desktop_workspace_focus_renders_workspace_panel`; commit `6dff031` |
| AW-003 | Done | Workspace GIF evidence | `assets/workspace-demo.gif` can be regenerated from VHS tape. | Docker VHS render; commits `ae04393`, `2fcdafd` |
| AW-004 | Done | Toggle Workspace focus | `a` opens Workspace and pressing `a` again returns to desktop. | `workspace_focus_toggle_returns_to_work_tab`; commit `a7169e5` |
| AW-005 | Done | Project selection | `j/k` and arrows move the selected Workspace project. | `workspace_project_selection_wraps_and_clamps`; commit `003883a` |
| AW-006 | Done | Selected project session drill-down | Workspace detail shows sessions, summaries, status, and redacted current tasks for the selected project. | `desktop_workspace_focus_renders_selected_project_sessions`; commit `ec20099` |
| AW-007 | Done | Workspace to session activation | `Enter` opens the selected project's first session in the sessions panel. | `activating_workspace_project_selects_its_first_session`; commit `7e00f95` |
| AW-008 | Done | Roadmap and task tracking | Add this tracker and keep roadmap status explicit. | `cargo test workspace`; this commit |
| AW-009 | Done | Workspace task state lens | Surface `.dw` active task title, phase, and decision counts more clearly. | `workspace_project_reads_dw_active_task_metadata`; `desktop_workspace_focus_renders_dw_task_lens`; this commit |
| AW-010 | Done | Workspace risk/attention queue | Sort and flag projects needing input, high context, open ports, or dirty git. | `workspace_attention_scores_and_sorts_projects`; `desktop_workspace_focus_renders_attention_signals`; this commit |
| AW-011 | Done | Workspace filter/lens controls | Add local-only lens cycling for all, attention, and `.dw` projects. | `workspace_lens_filters_navigation_to_matching_projects`; `desktop_workspace_focus_renders_lens_state`; this commit |
| AW-012 | Doing | Workspace timeline strip | Show recent selected-project tool calls and file access summaries without prompt/file contents. | UI tests with demo transcript data |
| AW-013 | Backlog | Snapshot/export surface | Add safe Markdown or JSON snapshot for sharing current Workspace state. | Snapshot tests with redaction assertions |
| AW-014 | Blocked | Mutating control actions | Dispatch/reply/restart/archive are intentionally blocked until read-only UX and audit story are stable. | Requires product decision and confirmation UX |

## Milestones

### M0: Baseline And Hygiene

Status: `Done`

- Windows local build/test/install is verified.
- Fork/upstream remote safety is documented.
- Opt-in diagnostics logging is available.
- EVD process exists for tests and GIF demo.

### M1: Read-Only Workspace MVP

Status: `Done`

- Project rollups are visible.
- `.dw` hints are detected.
- Project selection works.
- Selected project session drill-down works.
- Workspace can route into the sessions panel.

### M2: Workflow Intelligence

Status: `Doing`

- Improve `.dw` task/decision display.
- Add attention queue signals.
- Preserve read-only behavior and privacy boundaries.

### M3: Operator Controls

Status: `Blocked`

- Control actions stay blocked until the project has explicit confirmation,
  audit logging, and a clear product decision for which actions belong in this
  fork.

## Definition Of Done

For each Agentic Workspace slice:

- Code or documentation change is committed on `codex/agentic-workspace-mvp`.
- `cargo fmt -- --check` passes.
- `cargo clippy --all-targets --all-features -- -D warnings` passes for code
  changes.
- `cargo test` passes, or the final note explains why a narrower gate was used.
- `assets/workspace-demo.gif` is regenerated when the visible Workspace flow
  changes.
- This tracker is updated with status, EVD, and commit reference.

## Privacy Guardrails

- Do not display prompt text or file contents in Workspace.
- Current task text must stay redacted to tool name plus short argument.
- Paths may be shown only as local operational context and should be truncated
  in the UI.
- Snapshot/export work must default to safe-to-share output.
