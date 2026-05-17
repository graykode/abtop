# Compact Context

Use this file after chat compaction or when handing the repository to another
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

`upstream` push URL should remain disabled.

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
- support roadmap, handoff, and review,
- add automation only after audit, policy, and redaction gates are ready.

## Key Docs

Read in this order:

1. `AGENTS.md`
2. `docs/AGENT_HANDOFF.md`
3. `docs/EXECUTION_BOARD.md`
4. `docs/PRODUCTION_READINESS.md`
5. `docs/PRODUCTION_EVIDENCE.md`
6. `docs/ROADMAP_V2.md`
7. `docs/PRODUCT_STRATEGY.md`
8. `docs/UPSTREAM_SYNC.md`

## Current Done State

- Windows native build/test/install works.
- Claude StatusLine setup uses PowerShell.
- Claude quota handles UTF-8 BOM and no-BOM files.
- Codex quota is labeled as rate-limit remaining, not token count.
- Windows TCP port parsing handles real `netstat -ano -p TCP` rows.
- Task-aware Workspace is implemented.
- `.dw` task/project state is read into a safe internal model.
- Workspace has attention sorting, lens controls, timeline strip, session
  activation, task tree, roadmap signals, and handoff lanes.
- Safe exports exist:
  - `cargo run -- --workspace-summary`,
  - `cargo run -- --task-evidence`,
  - `cargo run -- --roadmap`,
  - `cargo run -- --handoff`,
  - `cargo run -- --handoff --json`.
- Cross-agent same-project validation with Claude Code + Codex is captured in
  `docs/PRODUCTION_EVIDENCE.md`.
- CI runs format, strict clippy, tests, and release build.
- Upstream OpenCode macOS cwd fix has been cherry-picked.

## Current Next Task

`P5-GTM-05`: release packaging and user-facing onboarding.

Goal:

- prepare a user-facing trial path,
- explain install/setup/first-run clearly,
- document the current production scope,
- document known limitations honestly,
- avoid making enterprise/cloud/auto-dispatch promises.

Suggested write scope:

- `README.md`,
- `docs/PRODUCTION_READINESS.md`,
- optional release notes or onboarding doc,
- `docs/EXECUTION_BOARD.md`.

EVD target:

```powershell
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
cargo run -- --help
cargo run -- --demo --handoff --json
```

## Local `.dw` And `.claude`

The repository currently has local `.dw/` and `.claude/` folders.

Rules:

- Treat them as local workflow assets unless the user explicitly asks to track
  them.
- Do not delete or overwrite them.
- Do not stage generated dw-kit framework files casually.
- If `.dw/tasks/ACTIVE.md` is stale, refresh via the user's dw workflow.
- Use `.dw` task state together with `docs/EXECUTION_BOARD.md`; do not let one
  silently contradict the other.

## Do Not Start Yet

- automatic task dispatch,
- direct agent-to-agent private chat,
- hosted dashboard,
- RBAC,
- cloud sync,
- upstream merge without following `docs/UPSTREAM_SYNC.md`.
