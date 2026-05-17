# Production Evidence

This file records sanitized evidence for production-readiness checks. Do not
include prompts, file contents, secrets, quota values tied to a private account,
or absolute local paths.

## 2026-05-17: Same-Project Claude Code + Codex Validation

Scope:

- project basename: `agentic-interview-web`,
- `.dw` task state present,
- Claude Code and Codex were both running against the same project,
- validation used local collectors only.

Commands:

```powershell
cargo run -- --doctor
cargo run -- --workspace-summary
cargo run -- --roadmap
cargo run -- --handoff
cargo run -- --handoff --json
```

Observed results:

- doctor completed with 0 errors; Claude quota can warn as stale when no recent
  Claude response has refreshed the StatusLine data,
- workspace summary reported 1 project and 2 sessions,
- cross-agent handoff JSON reported one project with both `claude` and `codex`
  in `active_agents`,
- roadmap reported ready work and no blocked branches,
- handoff Markdown showed assignment queue and live coordination notes for both
  agents,
- exports did not include prompt text, file contents, secrets, or absolute local
  paths.

Issue found and fixed:

- Before the fix, Claude Code and Codex sessions for the same project could be
  split into duplicate Workspace projects when their `cwd` strings used
  different path representations.
- The fix adds a canonical Workspace identity key and uses it for project
  grouping, session matching, graph construction, evidence bundles, handoff
  exports, and TUI Workspace details.

Production conclusion:

- The local Agentic Workspace GTM baseline is validated for the same-project
  Claude Code + Codex coordination flow.
