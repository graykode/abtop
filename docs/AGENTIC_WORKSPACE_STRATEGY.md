# Agentic Workspace Strategy

This document turns the fork from an agent monitor into a staged agentic
workspace while keeping the first implementation read-only and safe.

## Goal

Build a friendly command center for multi-agent development:

- observe running agents and their local impact,
- understand work by project, task, risk, and decision,
- coordinate multiple sessions without losing context,
- eventually act on sessions through explicit, auditable controls.

## Research Inputs

### Anthropic Agent View

Agent View introduces a control surface for background Claude Code sessions:
state grouping, needs-input queues, dispatch, peek/reply, attach, pinning,
renaming, and session ordering. The product pattern is a supervisor-oriented
inbox, not just a process list.

Useful translation for this fork:

- group by actionable state,
- surface "needs input" and blocked work first,
- keep attach/control actions explicit,
- preserve a read-only baseline before adding mutation.

### the-great-flow

`demo-v2.0.html` uses a spatial temporal graph with modes and lenses:
Explore, Search, Story, Causal, Forecast, swimlanes, minimap, guided journeys,
forecast voting, help, responsive panels, and stress testing.

Useful translation for this fork:

- turn projects/tasks/agents into a workspace graph over time,
- add lenses such as Agents, Projects, Tasks, Risks, Decisions,
- use swimlanes for project or workflow state,
- use causal chains for "this agent touched X because Y, affecting Z",
- use guided journeys for issue-to-PR or debugging flows.

### Understand Anything

Understand Anything contributes the "understanding layer": codebase graph,
semantic search, guided tours, domain views, and impact analysis.

Useful translation for this fork:

- workspace map should explain why work matters, not just that work exists,
- file access and git changes can become lightweight impact signals,
- a future semantic layer can sit behind the TUI without making the TUI heavy.

### Hermes Agent

Hermes emphasizes persistent skills, memory, toolsets, scheduling, terminals,
MCP integration, and agent growth over time.

Useful translation for this fork:

- model capabilities and toolsets as first-class workspace data,
- keep memory and recurring jobs visible but local,
- integrate MCP data as part of the workspace, not a separate afterthought.

### dw-kit / dv-workflow

`dw-kit` provides the workflow governance layer: Initialize, Understand, Plan,
Execute, Verify, Close, plus task docs, ADRs, telemetry, dashboards, and quality
gates.

Useful translation for this fork:

- detect `.dw` workflow state in project roots,
- surface active task and decision counts,
- treat guards, records, surfaces, bridges, and tunes as workspace dimensions,
- use dw artifacts as a bridge between agent sessions and human planning.

## Product Model

The workspace has four layers:

1. Observe: current agents, status, tokens, context, ports, git state.
2. Understand: projects, touched files, task phase, decisions, risks.
3. Coordinate: grouping, filters, pinning, blockers, review-ready queues.
4. Act: dispatch, attach, reply, stop, restart, archive.

The current implementation starts at layers 1 and 2.

## MVP Scope

Read-only Agentic Workspace:

- Add a `Workspace` narrow tab.
- Aggregate live sessions by project.
- Prioritize active projects first.
- Show active/waiting/blocked counts.
- Show max context, token total, git changes, child ports.
- Detect `.dw` workflow hints:
  - `.dw/tasks/ACTIVE.md` or `.dw/ACTIVE.md`,
  - `.dw/decisions/*.md`.

Non-goals for MVP:

- no dispatch,
- no reply/attach,
- no mutation of `.dw`,
- no prompt/file-content display,
- no graph UI yet.

## Safety Boundaries

- Default to read-only.
- Do not log prompt text, transcript lines, file contents, tokens, or secrets.
- Show local paths sparingly and truncate in UI.
- Make future control actions confirmable and auditable.

## Next Slices

1. Workspace tab, project rollup, `.dw` hints.
2. Search/filter by workspace state.
3. Task/decision detail pane.
4. Workspace timeline from tool calls and git changes.
5. Graph or swimlane mode after the TUI data model is stable.
6. Explicit control actions after read-only UX is proven.

