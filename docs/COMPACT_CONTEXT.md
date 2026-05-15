# Compact Context

Use this file when resuming after chat compaction or handing work to another
agent.

## Branch

Current product branch:

```text
codex/agentic-workspace-mvp
```

Remote:

```text
origin   https://github.com/huygdv/abtop.git
upstream https://github.com/graykode/abtop.git
```

`upstream` push URL should remain `DISABLED`.

## Product Direction

abtop is evolving from an agent monitor into a local-first Agentic Workspace.

Core thesis:

```text
dw-kit task graph + abtop runtime graph = agentic work graph
```

North star:

- observe agent work,
- connect it to project/task state,
- produce safe evidence,
- support handoff and review,
- add mutating controls only after audit and confirmation are ready.

## Key Docs

Read in this order:

1. `AGENTS.md`
2. `docs/EXECUTION_BOARD.md`
3. `docs/AGENT_HANDOFF.md`
4. `docs/ROADMAP_V2.md`
5. `docs/PRODUCT_STRATEGY.md`
6. `docs/UPSTREAM_SYNC.md`

## Current Done State

- Windows native build/test/install works.
- Claude StatusLine setup uses PowerShell.
- Claude quota handles UTF-8 BOM and no-BOM files.
- Codex quota is labeled as rate-limit remaining, not token count.
- Windows TCP port parsing handles real `netstat -ano -p TCP` rows.
- Read-only Agentic Workspace MVP exists.
- Workspace has attention sorting, lens controls, timeline strip, session
  activation, and safe Markdown export.
- Upstream OpenCode macOS cwd fix has been cherry-picked.

## Current Next Task

`P1-T01`: dw-kit task index reader.

Goal:

- parse dw-kit task/project metadata into a safe internal model,
- keep it read-only,
- avoid prompt text and file contents,
- prepare Workspace task detail pane v2.

Suggested write scope:

- `src/task/mod.rs`,
- `src/task/dw.rs`,
- small integration in `src/app.rs`,
- tests and docs.

EVD target:

```powershell
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test task
cargo test workspace
```

## Do Not Start Yet

- dispatch/reply/restart/archive controls,
- mutating `.dw` writes,
- generic mind-map UI without task/runtime graph model,
- upstream merge without following `docs/UPSTREAM_SYNC.md`.
