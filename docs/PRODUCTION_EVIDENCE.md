# Production Evidence

This file records sanitized evidence for production-readiness checks. Do not
include prompts, file contents, secrets, quota values tied to a private account,
or absolute local paths.

## 2026-05-18: v0.5.0 Composer + Dispatch Pipeline (Synthetic Gate)

Scope:

- version bump `0.4.4` → `0.5.0` for the `P6-UX-01` composer feature,
- automated production gate run against the bumped commit,
- synthetic dispatch validation via `--demo --dispatch-task` (real-session
  manual gate per `docs/PRODUCTION_READINESS.md` § *Required Manual Gate*
  step 8 is deferred to the operator running a live `.dw` project with
  `allow_dispatch_claude = true`).

Commands:

```powershell
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
cargo run -- --version
cargo run -- --demo --workspace-summary
cargo run -- --demo --handoff --json
cargo run -- --demo --dispatch-task dataset-drift-guardrails --dispatch-dry-run
```

Observed results:

- formatting + clippy clean,
- `cargo test`: 248 passed, 0 failed,
- `cargo run -- --version` reports `abtop 0.5.0`,
- workspace summary and handoff JSON exports redact prompt/file/path
  content as before,
- headless dispatch exits `0`, reports `outcome: dry-run`, prints the
  expected redacted brief, audit event recorded under `{audit_dir}`,
- no real spawn occurred (verified by absence of child claude/codex
  process and `response bytes: 0`).

Coverage gap:

- real-session manual gate against `claude --print` / `codex exec` is the
  operator's responsibility before tagging the release; this file records
  only the automated + demo evidence.

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
