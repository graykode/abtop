# Module: task_graph

## Vai trò

Data model for the workspace graph: nodes (Project, Task, DecisionSet, RecordSet, Verification, Agent, Risk) and typed edges (Contains, ActiveTask, DependsOn, WorkedBy, HasRisk, …). Feeds visual surfaces and the evidence/roadmap exports — a *data model*, not a renderer.

## Files chính

| File | Vai trò |
|------|---------|
| `mod.rs` (~13KB) | `GraphNode`, `GraphEdge`, `GraphNodeKind`, `GraphEdgeKind`, `TaskGraph` builder + queries |

## Public API / Exports

- `GraphNodeKind` enum: `Project | Task | DecisionSet | RecordSet | Verification | Agent | Risk`
- `GraphEdgeKind` enum: `Contains | ActiveTask | HasDecisionSet | HasRecordSet | HasVerification | DependsOn | WorkedBy | HasRisk`
- `GraphNode { id, kind, label, weight }`, `GraphEdge { from, to, kind }`
- `TaskGraph` — graph container with build methods consuming `WorkspaceProject`/`WorkspaceTask` from `app`, plus `AgentSession` from `model`

## Dependencies

- **Upstream**: `crate::app::{WorkspaceProject, WorkspaceTask}`, `crate::model::{AgentSession, SessionStatus}`
- **Downstream**: `evidence/` (per-task bundles count `graph_nodes`/`graph_edges`/`dependency_count`), `app.rs` (workspace view + roadmap inputs), `ui/workspace.rs` (compact handoff lanes)

## Conventions riêng

- **No rendering here** — `task_graph` produces structural facts only; rendering (TUI tree, mind-map prototype) lives in `ui/`.
- **Edges are typed** — don't collapse to a single `Edge { from, to }`. Downstream consumers (evidence, roadmap, UI) discriminate on `GraphEdgeKind`.
- **Cyclic ownership with `app`**: graph types live in `task_graph`, but the input types they consume (`WorkspaceProject`, `WorkspaceTask`) live in `app.rs`. This is intentional MVP coupling — see `docs/ROADMAP_V2.md` P2 "Mind-map data model prototype".

## Lưu ý cho AI

- The graph is the data plane for the "agentic work graph" moat (`dw-kit task graph + abtop runtime graph`). Schema decisions here ripple into all visual surfaces and exports.
- Node IDs are stable strings (used as Markdown anchors in exports). Don't rename casually.
- Adding a new `GraphEdgeKind` requires updating downstream match arms in [[evidence]] (bundle counts) and `ui/workspace.rs` (lane rendering) — check both before merging.
- Recent commits: `a40de0a feat: add workspace task graph model`, `0b2fafa feat: add task dependency signals` (added DependsOn edges).
