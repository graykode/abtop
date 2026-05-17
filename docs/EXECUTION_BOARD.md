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

`P6-UX-01`: design and ship the task-aware dispatch composer. Pre-requisites
`P5-GTM-05` (release packaging) and `P4-DSP-01` (dispatch audit + policy
gates) are now Done; the composer can begin.

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
| P2-VIS-02 | Done | Codex | Visual task viewer | Mind-map data model prototype | Create graph nodes/edges for tasks, decisions, sessions, files, and risks. | P2-VIS-01 | `src/task_graph/*`, docs | `cargo test task_graph`; `cargo test workspace`; `cargo test`; `cargo clippy --all-targets --all-features -- -D warnings`; demo summary graph stats |
| P2-VIS-03 | Done | Codex | Visual task viewer | Task dependency roadmap signals | Parse task dependencies and expose first graph/UI signals for task order and blockers. | P2-VIS-02, user feedback | `src/task/*`, `src/app.rs`, `src/task_graph/*`, UI tests | `cargo test task`; `cargo test task_graph`; `cargo test workspace`; `cargo test`; `cargo clippy --all-targets --all-features -- -D warnings`; demo summary shows `deps=3` |
| P2-VIS-04 | Done | Codex | Visual task viewer | Roadmap sequencing view | Compute dependency-aware task order and surface ready/blocked/next task stages before agent assignment. | P2-VIS-03 | roadmap model, workspace UI/export tests | `cargo test roadmap`; `cargo test task`; `cargo test task_graph`; `cargo test workspace`; `cargo fmt -- --check`; `cargo clippy --all-targets --all-features -- -D warnings`; `cargo test`; `cargo build`; `cargo run -- --demo --workspace-summary` |
| P3-EVD-01 | Done | Codex | Evidence bundles | Per-task evidence bundle | Export safe per-task evidence: sessions, commands, files touched, checks, decisions. | P1-T03, P2-VIS-02 | export module, tests | `cargo test evidence`; `cargo test workspace`; `cargo test`; `cargo clippy --all-targets --all-features -- -D warnings`; `cargo run -- --demo --task-evidence` |
| P4-AUD-01 | Done | Codex | Controls | Local audit log | Add append-only audit log before any mutating control action. | Product decision | audit module, docs | `cargo test audit`; `cargo test`; `cargo clippy --all-targets --all-features -- -D warnings`; kill controls record audit events |
| P4-CTL-01 | Done | GPT-5.5 | Controls | Mutating control actions | Stop selected sessions and kill orphan ports require explicit confirmation, fresh process checks, dry-run support, clear outcomes, and audit events for requested/confirmed/skipped/blocked/sent/failed paths. | P4-AUD-01 | `src/app.rs`, `src/locale.rs`, `src/main.rs`, README | `cargo test audit` exit 0; `cargo test workspace` exit 0; `cargo test control` exit 0; `cargo fmt -- --check` exit 0; `cargo clippy --all-targets --all-features -- -D warnings` exit 0; `cargo test` exit 0; `cargo build` exit 0; `cargo run -- --help` exit 0; `cargo run -- --demo --once` exit 0; `cargo run -- --demo --workspace-summary` exit 0; `cargo run -- --demo --task-evidence` exit 0; `cargo run -- --doctor` exit 0 |
| P4-POL-01 | Done | Codex | Controls | Local control policy gates | Mutating controls can be disabled or scoped from local config before restart/archive/dispatch actions are added. | P4-CTL-01 | `src/config.rs`, `src/app.rs`, `src/main.rs`, README | `cargo test config`; `cargo test control`; `cargo test audit`; `cargo fmt -- --check`; `cargo clippy --all-targets --all-features -- -D warnings`; `cargo test`; `cargo build`; `cargo run -- --demo --workspace-summary` |
| P5-GTM-01 | Done | Codex | GTM workflow | Roadmap export CLI | Users can export a redacted dependency-aware roadmap before assigning agents to tasks. | P2-VIS-04 | `src/app.rs`, `src/main.rs`, demo, README | `roadmap_markdown_is_redacted_and_structured`; `cargo test roadmap`; `cargo run -- --demo --roadmap`; full validation suite |
| P5-GTM-02 | Done | Codex | GTM workflow | Agent assignment handoff | Turn roadmap stages and task evidence into safe human-readable and machine-readable handoff formats for humans or agents to claim next work. | P5-GTM-01, P3-EVD-01, user cross-agent feedback | `src/app.rs`, `src/main.rs`, README, strategy docs | `handoff_markdown_is_redacted_and_actionable`; `handoff_json_is_redacted_and_structured`; `cargo test handoff`; `cargo run -- --demo --handoff`; `cargo run -- --demo --handoff --json`; full validation suite |
| P5-GTM-03 | Done | Codex | GTM workflow | Visual assignment surface | Show Claude/Codex same-project coordination lanes in the TUI Workspace view, backed by the handoff model. | P5-GTM-02 | `src/ui/workspace.rs`, `src/ui/mod.rs`, README | `desktop_workspace_focus_renders_dw_task_lens`; `cargo test workspace`; `cargo fmt -- --check`; full validation suite |
| P5-GTM-04 | Done | Codex | GTM workflow | Real same-project validation | Run Claude Code and Codex against the same `.dw` project and capture non-demo handoff/roadmap evidence without prompt or file-content leaks. | P5-GTM-03 | `src/app.rs`, `src/ui/workspace.rs`, `src/task_graph/mod.rs`, `src/evidence/mod.rs`, docs | `workspace_projects_merge_canonical_same_directory_sessions`; live `--handoff --json` reported one project with both `claude` and `codex`; `cargo run -- --doctor`; `cargo run -- --workspace-summary`; `cargo run -- --roadmap`; `cargo run -- --handoff`; `cargo run -- --handoff --json` |
| P5-GTM-05 | Done | Claude | GTM workflow | Release packaging and onboarding | README Agentic Workspace section with walkthrough; consolidated user-facing limitations; fork release checklist covering pre-tag validation, contribute-upstream vs fork-only release options. | P5-GTM-04 | `README.md`, `docs/LIMITATIONS.md`, `docs/RELEASE_CHECKLIST.md` | `cargo fmt -- --check`; `cargo clippy --all-targets --all-features -- -D warnings`; `cargo test` (208 passed); `cargo build`; `cargo run -- --help`; `cargo run -- --demo --workspace-summary`; `cargo run -- --demo --roadmap`; `cargo run -- --demo --handoff`; `cargo run -- --demo --handoff --json`; `cargo run -- --demo --task-evidence`; README link check |
| P4-DSP-01 | Done | Claude | Controls | Dispatch action audit + policy | Extend `ControlPolicy` with `allow_dispatch_claude/codex/opencode` (opt-in, default `false`) + `is_dispatch_allowed`. Add `audit::actions`/`audit::outcomes` constants + `dispatch_action_for` helper + `DISPATCH_DRY_RUN_ENV` (`ABTOP_DISPATCH_DRY_RUN`). Infra-only — no UI, no real spawn. | P4-POL-01, P4-AUD-01 | `src/config.rs`, `src/audit/mod.rs`, `src/app.rs` (struct-init `..Default::default()`), focused tests | `cargo test config` (incl. `parse_dispatch_policy_keys`, `is_dispatch_allowed_matches_agent_cli_synonyms`, `dispatch_flags_default_false`); `cargo test audit` (incl. `dispatch_action_for_maps_known_agents`, `dispatch_event_uses_stable_vocabulary`, `dispatch_dry_run_env_name_is_stable`, `outcome_labels_match_kill_control_strings`); full production gate (see P5-GTM-05 EVD) |
| P6-UX-01 | Next | Unassigned | Composer | Task-aware dispatch composer | TUI composer in Workspace tab. Pick a task, preview safe handoff brief, choose agent, confirm, spawn one-shot `claude --print` / `codex exec` non-interactive call, capture response into evidence + audit. No keystroke injection into running CLI sessions. | P4-DSP-01, P5-GTM-02 | `src/app.rs`, `src/ui/workspace.rs` (new composer pane), evidence module | Pending |

## Completed Task Detail: P4-CTL-01

Target user:

- Users who need to stop runaway sessions, restart failed work, archive finished
  work, or dispatch a task without leaving the local monitoring workflow.

Pain solved:

- Mutating controls need a consistent confirmation and audit trail before abtop
  grows beyond read-only monitoring.

Hypothesis:

- Small audited controls let users recover from common agent failures while
  preserving local-only privacy and operator intent.

Data sources:

- current sessions, child processes, and orphan ports,
- `.dw` task/project metadata for archive or dispatch targets,
- append-only audit events from `P4-AUD-01`.

Privacy risk:

- Control labels can reveal local project names and task titles. Keep audit
  entries structured and avoid prompts, file contents, or transcript bodies.

Implemented design:

- Confirmation window for destructive actions.
- Audit requested, confirmed, skipped, blocked, dry-run, sent, and failed
  outcomes for existing kill workflows.
- Reuse fresh process and port checks before killing anything.
- Provide `ABTOP_CONTROL_DRY_RUN=1` for verified non-mutating test runs.

Deferred:

- Restart, archive, and dispatch remain blocked until local policy gates define
  when each mutating action is allowed.

Suggested write scope:

- `src/app.rs` and focused control helpers,
- `src/ui/*` confirmation/status surfaces,
- audit integration tests,
- focused tests.

EVD:

- `cargo test audit`,
- `cargo test workspace`,
- `cargo test control`,
- `cargo fmt -- --check`,
- `cargo clippy --all-targets --all-features -- -D warnings`,
- `cargo test`,
- `cargo build`,
- `cargo run -- --help`,
- `cargo run -- --demo --once`,
- `cargo run -- --demo --workspace-summary`,
- `cargo run -- --demo --task-evidence`,
- `cargo run -- --doctor`.

## Handoff Notes

- User feedback: large legacy conversions need dependency/pipeline planning
  before assigning agents to tasks.
- User feedback: Claude Code and Codex often run on the same project; the
  product should coordinate them through a shared task/evidence protocol before
  exploring direct agent-to-agent messaging.
- User feedback (2026-05-17): UX pain of switching between two CLI panes for
  Claude Code and Codex. Considered options A/B/C; chose option A — audited
  one-shot dispatch from prepared task context. Explicitly rejected direct
  keystroke injection (option B) and broadcast-to-both-agents chat (option C)
  for the current milestone.
- Avoid large refactors in `src/app.rs`; prefer extracting new task/workspace
  modules.
- Do not commit local EVD files that include private paths, quota, prompts, or
  screenshots.
- Current branch: `codex/agentic-workspace-mvp`.
