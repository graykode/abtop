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
- Workspace task/runtime view is implemented.
- Safe Workspace Markdown export exists.
- Dependency-aware roadmap export exists.
- Cross-agent handoff export exists in Markdown and JSON.
- Workspace TUI renders compact handoff lanes and assignment suggestions.
- Real same-project Claude Code + Codex validation is captured in
  `docs/PRODUCTION_EVIDENCE.md`.
- Upstream sync guide exists.
- Latest synced upstream fix: OpenCode macOS `lsof -a` cwd lookup.

Current next task:

`P5-GTM-05`: prepare release packaging and user-facing GTM onboarding from the
validated local Agentic Workspace baseline.

Why it matters:

- It turns the validated local baseline into something a real user can install,
  understand, and try without reading internal planning docs.
- It should make production scope and known limitations explicit.

Suggested first implementation:

- Use `docs/PRODUCTION_READINESS.md` and `docs/PRODUCTION_EVIDENCE.md`.
- Add a concise user-facing onboarding path for the fork.
- Include install, setup, first run, Workspace, roadmap, handoff, and limitations.
- Keep all docs in English and avoid private local paths.

Do not start:

- automatic dispatch/reply/restart/archive,
- direct agent-to-agent private chat,
- cloud/team sync before the policy and audit model is explicit.
