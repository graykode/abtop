# Agent Handoff Protocol

This document helps another agent resume work without reading the entire chat.

For a shorter resume brief, read `docs/COMPACT_CONTEXT.md`.

## First Five Minutes

1. Read `AGENTS.md`.
2. Read `docs/EXECUTION_BOARD.md`.
3. Read `docs/ROADMAP_V2.md`.
4. Check the current branch and worktree:

   ```powershell
   git status --short --branch
   git log --oneline -5
   ```

5. If the user asks for product direction, read `docs/PRODUCT_STRATEGY.md` and
   `docs/COMPETITIVE_MAP.md`.

## Claiming Work

Before changing files:

1. Pick one `Next` task from `docs/EXECUTION_BOARD.md`.
2. Change its status to `Doing`.
3. Set owner to your agent label, for example `Codex`.
4. Keep the write scope close to the task.
5. Update the board again before the final response.

If you cannot finish:

- leave status as `Doing`,
- add a handoff note with the blocker and next command,
- do not mark EVD as complete.

## Required Final State

For code changes:

```powershell
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

For docs-only changes:

- run targeted `rg` checks for the new links and terms,
- ensure docs are in English,
- keep the board updated.

For visible TUI changes:

- add or update tests,
- regenerate GIF only when the visible demo flow changes,
- never commit local EVD files containing private paths or real quota data.

## Privacy Rules

- Do not display prompt text or file contents in Workspace surfaces.
- Do not log transcript lines, secrets, or full tool inputs.
- Safe exports should use titles, counts, statuses, and redacted tool labels.
- Treat local screenshots and GIFs as private unless they use demo data.

## Git Rules

- Work on `codex/agentic-workspace-mvp` unless the user says otherwise.
- Keep `main` close to upstream.
- Use `docs/UPSTREAM_SYNC.md` for upstream merges.
- Do not rewrite pushed history.
- Do not revert user changes.

## Compact Context Brief

Current product thesis:

> abtop should become a local-first Agentic Workspace: a flight recorder,
> operations cockpit, task/workflow viewer, and safety layer for multi-agent
> software work.

Current moat:

```text
dw-kit task graph + abtop runtime graph = agentic work graph
```

Current technical state:

- Windows local setup works.
- Claude and Codex quota work and are labeled as remaining percent.
- Windows TCP port parsing is fixed.
- Workspace read-only MVP is implemented.
- Safe Workspace Markdown export exists.
- Upstream sync guide exists.
- Latest synced upstream fix: OpenCode macOS `lsof -a` cwd lookup.

Current next task:

`P1-T01`: build the dw-kit task index reader.

Why it matters:

- It connects live agent sessions to structured task state.
- It moves abtop from monitoring into task-aware Agentic Workspace territory.
- It strengthens the moat against provider-native dashboards.

Suggested first implementation:

- Add `src/task/` module.
- Parse safe metadata from `.dw` task files.
- Expose a task/project summary model.
- Integrate minimally into Workspace.
- Keep everything read-only.

Do not start:

- mutating control actions,
- dispatch/reply/restart/archive,
- generic mind-map UI without task/runtime data model.
