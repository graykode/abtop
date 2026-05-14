# Development Roadmap

This roadmap keeps the fork useful locally while preserving a clean path for
future upstream contributions.

## Baseline

The current fork can build, test, install, and run on native Windows.

Verified:

- Rust/MSVC build toolchain is working.
- `cargo fmt -- --check` passes.
- `cargo clippy --all-targets --all-features -- -D warnings` passes.
- `cargo test` passes with 137 tests.
- `cargo build --release` passes.
- `abtop --once` reads live local sessions.
- `abtop --demo --once` renders the README-style panel set.
- Branch `codex/windows-local-baseline` is pushed to the fork.

## Principles

- Keep `main` close to upstream.
- Develop on small feature branches.
- Prefer Windows fixes that do not regress macOS or Linux behavior.
- Protect local privacy: do not display prompt contents or file contents unless
  the existing product surface already does so intentionally.
- Keep realtime work cheap: fast UI redraw, bounded collector work, defensive
  parsing, and cached expensive operations.

## Phase 0: Fork Hygiene

Goal: make the fork easy to maintain.

- Keep `origin` as `huygdv/abtop`.
- Keep `upstream` as `graykode/abtop`.
- Keep `upstream` push URL disabled locally.
- Add and maintain Windows development documentation.
- Keep baseline DoD current when toolchain or behavior changes.

Status: in progress.

## Phase 1: Windows First-Class Support

Goal: remove Windows-specific rough edges before product work expands.

Priority work:

- Audit `abtop --setup` for native Windows.
- Replace or complement the shell-based StatusLine hook with a Windows-friendly
  command path.
- Verify Claude rate-limit file generation on Windows.
- Improve path and command display for Windows process trees.
- Validate port detection with common Windows dev servers.
- Add targeted tests for Windows path serialization and setup behavior.

Success criteria:

- `abtop --setup` works on native Windows without requiring Git Bash.
- Claude quota appears after a real Claude response when account data is
  available.
- Test coverage protects the Windows setup path.

## Phase 2: Operator UX

Goal: make the monitor more useful during daily multi-agent work.

Candidate features:

- Stronger filtering for project, agent type, status, and PID.
- Project grouping or workspace-focused view.
- Better Codex-specific timeline labeling.
- Export snapshot to JSON or Markdown.
- Config presets for hidden panels and hidden agents.
- Clearer empty and partial-data states.

Success criteria:

- Common workflows require fewer keystrokes.
- Snapshot export is safe to share by default.
- UI remains usable at 80x24 and polished at 120x40.

## Phase 3: Alerts And Automation

Goal: turn passive monitoring into useful operational signals.

Candidate features:

- High context warning thresholds.
- Rate-limit nearing-cap indicators.
- Orphan-port alerting.
- Stale session detection.
- Optional local notification hooks.

Success criteria:

- Alerts are local-only and configurable.
- No network service is required.
- Alert states are visible in both TUI and snapshot output.

## Phase 4: Release Management

Goal: create reproducible fork releases when needed.

Candidate work:

- Fork-specific version tags, such as `huygdv-v0.4.4-win1`.
- Windows release binaries.
- Changelog for fork changes.
- Scheduled dependency audit.
- Decide which changes are candidates for upstream pull requests.

## Risk Register

- `abtop --setup` is not yet native Windows friendly.
- `cargo audit` reports transitive warnings through `ratatui 0.29.0`.
- Collector logic depends on internal Claude/Codex/OpenCode file formats.
- Interactive TUI verification needs a real terminal, not a non-interactive pipe.
- Session data can contain sensitive local paths and operational metadata.

