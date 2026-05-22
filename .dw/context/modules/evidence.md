# Module: evidence

## Vai trò

Builds and renders per-task **safe** evidence bundles: counts, agent activity, tools, files touched, risks, decisions, verification status. Privacy-critical — this is the surface a user shares with a reviewer or hands off to another agent.

## Files chính

| File | Vai trò |
|------|---------|
| `mod.rs` (~14KB) | `TaskEvidenceBundle`, `EvidenceAgent`, `build_task_evidence()`, `render_task_evidence_markdown()` |

## Public API / Exports

- `TaskEvidenceBundle` — project, task, status, phase, next_action, acceptance/verification/decision/record/graph counts, dependency_count, agents, tools, files, risks
- `EvidenceAgent` — source (e.g. `claude`/`codex`), status, current_tool
- `build_task_evidence(projects, sessions, graph) -> Vec<TaskEvidenceBundle>`
- `render_task_evidence_markdown(bundles) -> String`

## Dependencies

- **Upstream**: `crate::app::{WorkspaceProject, WorkspaceTask}`, `crate::model::{AgentSession, FileAccess, SessionStatus, ToolCall}`, `crate::task_graph::TaskGraph`
- **Downstream**: `app.rs` (`build_task_evidence`, `render_task_evidence_markdown` re-exported), `main.rs` (`--task-evidence` CLI flag)

## Conventions riêng

- **Counts and titles only**, never bodies. Tool labels are redacted before inclusion (no `Edit src/secrets.rs` content, just `Edit` + path).
- **Filter to dw-projects**: `build_task_evidence` only iterates `projects.iter().filter(|p| p.has_dw)` — non-dw projects are skipped.
- **Stable output ordering**: bundles are sorted for deterministic Markdown rendering (so diffs against CI fixtures stay meaningful).
- **No absolute paths**: file lists strip user-home and absolute prefixes before rendering.

## Lưu ý cho AI

- **Privacy is the whole point of this module.** If you're tempted to add a "preview" field or a body excerpt, stop — that's the line `docs/AGENT_HANDOFF.md` "Privacy Rules" forbids. Counts, titles, statuses, redacted tool labels only.
- Tests live near the module and assert that rendered Markdown is **redacted and structured** (see `workspace_summary_markdown_is_redacted_and_structured`, `handoff_markdown_is_redacted_and_actionable`). Run `cargo test evidence` after any change.
- Cross-agent handoff (Markdown + JSON) shares this module's redaction principles — see commits `92227bb feat: add cross-agent handoff export`, `9856556 feat: add structured handoff export`.
- Recent same-project merging fix (`784d0d8`) means two agents on the same project become one bundle row — preserve that when refactoring.
