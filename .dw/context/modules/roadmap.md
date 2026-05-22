# Module: roadmap

## Vai trò

Computes a **dependency-aware** roadmap plan for a workspace: groups tasks into Ready/Next/Last stages based on dependency graph + task status, surfaces risks, and powers the `--roadmap` export and the cross-agent handoff assignment surface.

## Files chính

| File | Vai trò |
|------|---------|
| `src/roadmap.rs` (~10KB) | `RoadmapPlan`, `RoadmapStage`, `RoadmapStageLabel`, `RoadmapTask`, `RoadmapRisk`, `build_project_roadmap()` |

## Public API / Exports

- `RoadmapPlan { ready_count, blocked_count, stages, risks }`
- `RoadmapStage { index, label: RoadmapStageLabel, tasks }`
- `RoadmapStageLabel` enum: `First | Next | Last` (with `as_str()` → `"first" | "next" | "last"`)
- `RoadmapTask { title, status, … }`
- `RoadmapRisk` — risk surface
- `build_project_roadmap(projects, …) -> RoadmapPlan`

## Dependencies

- **Upstream**: `crate::app::{WorkspaceProject, WorkspaceTask}`, `crate::task::TaskStatus`, `std::collections::{BTreeSet, HashMap}`
- **Downstream**: `app.rs` (roadmap panel + handoff assignment), `main.rs` (`--roadmap` CLI), `ui/workspace.rs` (assignment lanes)

## Conventions riêng

- **Stage labels are coarse on purpose**: just `first`/`next`/`last` — keeps the export readable. Don't bikeshed into 7-stage models without a product reason.
- **Dependency cycles are tolerated, not preferred**: if a cycle is detected, tasks fall into the highest-priority stage they belong to — better than crashing. Add a `RoadmapRisk` instead of panicking.
- **Ready vs Blocked count drives the dashboard headline** — both fields are exposed at the `RoadmapPlan` top level for fast access.

## Lưu ý cho AI

- This module is the planning brain behind handoff (`P5-GTM-02`). Changing stage semantics breaks the `handoff_markdown_is_redacted_and_actionable` test — re-read those expectations first.
- The "Ready next / Blocked / Needs review" categorization is the user-visible promise from `docs/ROADMAP_V2.md` P1/P3.5. Don't change category names without updating the export tests and the strategy docs.
- Recent commits: `350a58f feat: add roadmap export and control policy gates`, `baf860f feat: add dependency-aware roadmap sequencing`.
- See [[task]] for `TaskStatus` source-of-truth and [[task_graph]] for the dependency edge data.
