# Product Strategy

This document frames the fork as a product, not only as a terminal utility.
The goal is to make future engineering work trace back to a real customer pain,
a differentiated wedge, and a credible business path.

## Product Thesis

AI coding agents are moving from short assistant turns to long-running software
work. The more agents a developer or team runs, the more they lose operational
clarity: what is running, what changed, what is blocked, what opened ports, what
used quota, and what evidence is safe to share.

abtop can become the local-first control tower for agentic software work:

- a flight recorder for every local agent session,
- an operations cockpit for current project health,
- a task/workflow viewer backed by structured project artifacts,
- a shared workspace protocol where Claude Code, Codex, OpenCode, and future
  agents coordinate through task state, evidence, blockers, and handoffs,
- a safety layer before agent actions become mutating or automated.

The initial wedge is still simple: run `abtop` and immediately understand what
your coding agents are doing. The long-term product is an Agentic Workspace.

## Ideal Customer Profiles

### Solo Power User

Runs multiple CLI agents locally and wants to move faster without losing track.

Pain:

- many terminals and sessions,
- no single view of active work,
- rate limits and context pressure are hard to connect to tasks,
- difficult handoff from one agent/session to another.

Willingness to pay:

- modest but fast feedback loop,
- likely to pay for Pro features if the tool saves time daily.

### Small Engineering Team

Uses Claude Code, Codex, Cursor, OpenCode, or similar tools across several
repos and developers.

Pain:

- work history is scattered across terminals and chat transcripts,
- reviewers cannot easily see what an agent did,
- managers cannot see blocked/ready/risky work without asking humans,
- task state and agent state live in separate systems.

Willingness to pay:

- higher if abtop provides safe reports, task handoff, and local audit trails.

### Enterprise Platform / Security Team

Wants agents, but cannot approve uncontrolled local automation.

Pain:

- no trustworthy audit layer,
- unclear process ownership,
- local processes and ports can survive agent sessions,
- security teams need policy and evidence, not just productivity claims.

Willingness to pay:

- highest if self-hosted, local-first, and policy-aware.

## Pain Points To Own

1. Loss of control over many concurrent agents.
2. Lack of trustworthy evidence after an agent run.
3. Weak connection between tasks, decisions, files, commands, ports, and quota.
4. Local chaos from child processes, dev servers, and orphan ports.
5. Unsafe handoff between humans and agents.
6. Weak coordination when multiple agent CLIs work inside the same project.
7. Enterprise trust gap for agentic software engineering.

## Positioning

Short version:

> Run many coding agents without losing control.

Long version:

> abtop is a local-first Agentic Workspace that turns terminal-based coding
> agents into observable, auditable, task-aware software work.

Avoid positioning the product as only "btop for agents" long-term. That phrase
is an excellent wedge, but the product should graduate toward control,
provenance, and workflow intelligence.

## dw-kit Role

dw-kit should become the structured workflow substrate underneath the Agentic
Workspace.

abtop observes the runtime layer:

- agents,
- sessions,
- tool calls,
- commands,
- files,
- ports,
- rate limits,
- context pressure.

dw-kit provides the planning and governance layer:

- active tasks,
- phases,
- decisions,
- verification records,
- acceptance criteria,
- project rituals,
- closeout evidence.

Together, they form a stronger system:

```text
dw-kit task graph + abtop runtime graph = agentic work graph
```

This is a meaningful product moat because most agent dashboards show only run
telemetry, while most task managers know nothing about live agent behavior.

## Cross-Agent Coordination

Users increasingly run Claude Code and Codex on the same project. The product
should not depend on agents chatting freely with each other. That creates
unclear authority, token waste, and hard-to-audit context drift.

The safer product shape is a shared workspace protocol:

- dw-kit owns task intent, dependency order, status, and acceptance criteria,
- abtop observes each agent's runtime evidence, current task, touched files,
  commands, ports, context pressure, and blockers,
- handoff exports tell a human or a next agent what can be claimed, what must
  wait, what evidence exists, and which agent type is a reasonable fit,
- direct agent-to-agent messaging can be explored later as an optional layer on
  top of the shared protocol, not as the source of truth.

This gives users the outcome they ask for: Claude Code and Codex can cooperate
on one project without needing a fragile private conversation channel.

## Task Manager Direction

The task manager should not be a generic Kanban clone. It should be an
agent-native project map.

Recommended views:

- **Task Tree**: project, task, subtask, acceptance criteria, status.
- **Mind Map**: task dependencies, blockers, owners, and next-ready work.
- **Run Timeline**: which agents touched which tasks and files.
- **Decision Map**: ADRs and key decisions connected to tasks.
- **Risk Lens**: high context, stale agents, failed commands, orphan ports,
  dirty git, missing tests.
- **Handoff View**: current state, next action, evidence, safe export.

Nogic is a useful reference for interactive workspace maps that explain code
structure and relationships. The abtop opportunity is adjacent but different:
map live agent work and task state, not only code comprehension.

Mind-map task managers validate the visual planning need, but many are weak at
execution provenance. abtop can win by linking map nodes to actual runtime
evidence.

## Moat

The moat should be built around data integration and trust, not just UI.

- **Cross-agent local telemetry**: Claude Code, Codex, OpenCode, and future CLI
  agents.
- **Task/runtime fusion**: dw-kit task artifacts connected to live agent
  sessions.
- **Agent handoff protocol**: dependency-aware, evidence-backed work packages
  that let different agent CLIs continue each other's work safely.
- **Privacy-first export**: safe summaries without prompt/file-content leaks.
- **Evidence format**: shareable snapshots that explain work state and risk.
- **Local policy controls**: explicit confirmation and audit before mutation.
- **Workflow memory**: decisions, verification, task phase, and history survive
  individual agent sessions.

## Business Paths

### Open-Core

Free:

- local TUI,
- live sessions,
- quota/context/ports,
- safe snapshot export.

Pro:

- searchable history,
- task mind map,
- richer dw-kit integration,
- configurable redaction,
- evidence bundles,
- local web dashboard.

### Team

- shared workspace reports,
- team policy presets,
- project health dashboards,
- audit logs,
- task handoff workflows.

### Enterprise

- self-hosted control plane,
- RBAC and policy packs,
- approved action workflows,
- compliance-grade audit,
- integration with existing task and source systems.

## Acquisition Hypothesis

Potential acquirers would not buy a TUI alone. They may care about:

- cross-agent local telemetry,
- agentic work provenance,
- task/runtime graph,
- privacy-first governance,
- developer adoption among power users.

Potential strategic buyers include agentic IDEs, terminal companies,
developer-workspace platforms, code intelligence platforms, and observability
vendors.

## Validation Plan

1. Interview 10 heavy Claude Code/Codex users.
2. Ask for screenshots of their current multi-agent workflow.
3. Identify where they lose context, money, time, or trust.
4. Test whether a safe workspace snapshot is useful enough to share.
5. Test whether dw-kit task integration makes agent work easier to resume.
6. Test whether a visual task map is clearer than the current session table.
7. Only then add mutating controls.

## Strategic Rule

Every new feature should answer at least one of these:

- Does it reduce loss of control?
- Does it create trustworthy evidence?
- Does it connect agent runtime to task/project state?
- Does it improve handoff or review?
- Does it make the product harder for single-provider dashboards to copy?
