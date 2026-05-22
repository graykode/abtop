# Module: task

## Vai trò

Reads dw-kit task state from a project's `.dw/tasks/**` directory and produces a safe internal model (`DwTaskSummary` + normalized `TaskStatus`). This is the *moat surface* — it's what turns abtop from a session monitor into a task-aware workspace.

## Files chính

| File | Vai trò |
|------|---------|
| `dw.rs` (~15KB) | Parser. `TaskStatus` enum + label/from_label normalization. `read_project_state()` walks `.dw/` and returns summaries |
| `mod.rs` (~100 bytes) | Re-exports `dw` submodule |

## Public API / Exports

- `TaskStatus` enum: `Ready | Doing | Blocked | Review | Done | Unknown` (`#[default] = Unknown`)
  - `TaskStatus::label()` → `"ready" | "doing" | "blocked" | "review" | "done" | "unknown"`
  - `TaskStatus::from_label(value)` — case-insensitive normalization, strips quote/bracket noise, maps many synonyms
- `DwTaskSummary` — task data type (title, status, phase, acceptance criteria count, decisions, verification status, dependencies, next action)
- `read_project_state(dw_dir)` — top-level entry; called by `app.rs` per workspace project

## Dependencies

- **Upstream**: `std::fs`, `std::path` only — no internal crate deps. Designed as a pure parser.
- **Downstream**: `app.rs` (`read_project_state`, `DwTaskSummary`, `TaskStatus`), `roadmap.rs`, `task_graph` (via `WorkspaceTask` in `app.rs`)

## Conventions riêng

- **Status normalization is generous**: `"in progress"`, `"in-progress"`, `"active"`, `"started"` → `Doing`; `"todo"`, `"to do"`, `"next"`, `"pending"` → `Ready`; etc. Unknown labels fall back to `TaskStatus::Unknown` (never panic).
- **Defensive trimming**: `from_label` strips `"`, `'`, `` ` ``, `[`, `]` before matching — handles common Markdown table noise.
- **Read-only**: this module never writes to `.dw/`. Mutation of task state is out of scope for the current milestone (per ROADMAP_V2 P4: mutating actions need audit + policy first).
- Tests at `task::dw::tests::*` cover status mapping + next-action derivation.

## Lưu ý cho AI

- **Privacy contract**: task files may contain prompt-like text. The parser must surface counts, titles, statuses, and structural metadata — NOT raw body content — to its callers. Any export consumer should re-verify before rendering.
- **Schema is dw-kit's**, not ours. If dw-kit ships a machine-readable task index in the future (per `docs/ROADMAP_V2.md` P1), prefer migrating over scraping Markdown. Until then, stay lenient.
- **No assumptions about file layout** beyond `.dw/tasks/`. Some projects use the legacy 3-file shape (`context.md`/`plan.md`/`progress.md`); v2 uses 2 files (`spec.md`/`tracking.md`). Parser should accept both.
- This is the surface where the "agentic work graph" moat lives (`dw-kit task graph + abtop runtime graph = agentic work graph` — per `docs/AGENT_HANDOFF.md`). Breaking changes here ripple into [[task_graph]], [[evidence]], [[roadmap]], and every workspace export.
- Recent commits in this module: `0b2fafa feat: add task dependency signals`, `68a7e1b feat: add dw task-aware workspace state` — the dependency-signal work is what feeds the roadmap planner.
