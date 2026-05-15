# Roadmap V2

This roadmap organizes future work around product value. It complements
`docs/ROADMAP.md`, which still tracks platform and implementation hygiene.

## North Star

Make agentic software work observable, task-aware, and safe to hand off.

The product should help a user answer:

- What is happening now?
- Which task or project does it affect?
- What changed?
- What is risky or blocked?
- What should happen next?
- What evidence can I safely share?

## Milestone P0: Trustworthy Local Baseline

Status: mostly done.

Outcomes:

- Windows local install works.
- Claude/Codex/OpenCode sessions are visible.
- Claude and Codex quota are understandable.
- Windows ports are detected correctly.
- Safe workspace snapshot exists.
- Diagnostics can prove which data sources were loaded.

Remaining:

- harden Windows kill/orphan-port safety,
- improve Windows command/path display,
- add a manual EVD checklist for real local ports and quota.

## Milestone P1: Task-Aware Workspace

Goal:

Make abtop understand not only sessions, but the work those sessions belong to.

Key outcomes:

- Read dw-kit task state as a first-class data source.
- Show active task, phase, acceptance criteria, decisions, and verification
  status per project.
- Connect live sessions to task nodes.
- Show "ready next", "blocked", "needs review", and "needs human input".
- Keep the first version read-only.

Potential data sources:

- `.dw/tasks/ACTIVE.md`,
- `.dw/tasks/*.md`,
- `.dw/decisions/*.md`,
- `.dw/records/*.md`,
- future dw-kit machine-readable task index.

Definition of done:

- user can open abtop and know which project task each active agent is advancing,
- no prompt text or file contents are exposed,
- snapshot export includes task state safely.

## Milestone P2: Visual Task Viewer

Goal:

Add a visual surface that makes complex agentic work easier to reason about
than a table.

Recommended first implementation:

- terminal-friendly structured tree first,
- then local web or TUI canvas after the data model stabilizes.

Views:

- task tree,
- mind map,
- dependency graph,
- decision map,
- agent-run timeline,
- risk lens.

Nogic-inspired principle:

- use a canvas/map to explain relationships,
- keep narrative focus and progressive disclosure,
- link nodes back to concrete runtime evidence.

Anti-goals:

- do not build a generic mind-map toy,
- do not require users to abandon their existing task system,
- do not mutate task files until the audit story is ready.

Definition of done:

- user can see task dependencies and agent activity in one view,
- user can identify the next-ready task and blocked branches quickly,
- user can export a safe task/workspace snapshot.

## Milestone P3: Evidence Bundles

Goal:

Make agent work reviewable and handoff-ready.

Outputs:

- project workspace summary,
- per-task evidence bundle,
- changed files summary,
- command/test timeline,
- ports and process impact,
- decisions and verification status,
- redaction report.

Definition of done:

- a reviewer can understand what happened without opening raw transcripts,
- a teammate can resume the task with less context loss,
- sensitive content stays out by default.

## Milestone P4: Local Policy And Controls

Goal:

Allow useful actions without creating an unsafe agent control panel.

Actions to consider:

- kill orphan ports,
- stop selected session,
- archive finished sessions,
- open task evidence,
- mark task blocked/ready/review,
- dispatch a prompt from a prepared task context.

Required before mutation:

- confirmation UX,
- dry-run preview,
- local audit log,
- policy config,
- rollback or recovery guidance where possible.

Definition of done:

- every mutating action is explicit and auditable,
- workspace state explains who/what triggered the action,
- unsafe actions can be disabled by policy.

## Milestone P5: Commercial Surface

Goal:

Validate whether abtop can become more than a local utility.

Experiments:

- Pro local history and search,
- task mind-map viewer,
- shareable evidence reports,
- team-safe redaction presets,
- local web dashboard,
- enterprise self-hosted audit mode.

Pricing hypotheses:

- Solo Pro: pay for daily clarity and history.
- Team: pay for handoff, review, and shared project evidence.
- Enterprise: pay for governance, audit, and policy.

## Prioritized Next Engineering Slices

1. Windows orphan-port kill safety.
2. Windows command/path display polish.
3. dw-kit task index reader.
4. Workspace task detail pane v2.
5. Safe task evidence export.
6. Task tree view in TUI.
7. Mind-map/data model prototype.
8. Local audit log for future controls.

## Product Gate

Before starting a feature, write the answer:

- Target user:
- Pain solved:
- Why now:
- Data source:
- Privacy risk:
- Evidence:
- Moat contribution:
