# Production Readiness

This checklist defines the current production gate for the Agentic Workspace
fork. It is intentionally local-first and privacy-preserving.

## Required Automated Gate

Run from the repository root:

```powershell
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build
cargo run -- --help
cargo run -- --demo --workspace-summary
cargo run -- --demo --roadmap
cargo run -- --demo --handoff
cargo run -- --demo --handoff --json
```

Expected outcome:

- formatting passes,
- clippy passes with warnings denied,
- all tests pass,
- build completes,
- demo exports do not include prompt text, file contents, or absolute demo
  paths,
- `--handoff --json` returns valid JSON with schema
  `abtop.agent_handoff.v1`.

## Required Manual Gate

1. Run `cargo run -- --doctor` and resolve hard failures.
2. Run `cargo run -- --setup` if Claude quota should be shown.
3. Start at least one Claude Code or Codex session in a `.dw` project.
4. Run `cargo run` and press `a` to open Workspace.
5. Verify the selected `.dw` project shows:
   - task status and phase,
   - roadmap ready/blocked/stage counts,
   - handoff lanes for `claude-code`, `codex-cli`, and `opencode`,
   - assignment rows with compact agent-fit hints,
   - blocked/risky work under hold notes.
6. Run `cargo run -- --handoff` and `cargo run -- --handoff --json`.
7. Confirm no prompt text, file contents, secrets, or absolute local paths are
   present in exported output.

## Production Definition

The current production-ready scope is:

- local session monitoring for Claude Code, Codex CLI, and OpenCode,
- read-only task/runtime workspace intelligence,
- dependency-aware roadmap export,
- cross-agent handoff export in Markdown and JSON,
- compact TUI assignment lanes in Workspace,
- audited and policy-gated destructive controls.

Out of scope for the current production gate:

- remote or team cloud sync,
- automatic task dispatch,
- direct agent-to-agent private chat,
- RBAC,
- hosted dashboards.

Those can be added later only after policy, audit, and redaction gates are
designed first.
