# Release Checklist

Pre-release validation for the Agentic Workspace fork. Run this before
cutting any release artifact — fork binary, upstream contribution, or tag.

The upstream release mechanics (cargo-dist + tag-triggered workflows + the
crates.io publish) live in `AGENTS.md` § *Release Process*. This file covers
the **fork-specific validation** that has to pass before triggering those.

## 1. Branch + Working Tree Hygiene

- [ ] Current branch is `codex/agentic-workspace-mvp` (or release branch off it).
- [ ] `git status --short --branch` shows a clean tree (or only the intended
      release-related changes).
- [ ] No untracked local EVD files contain private paths, prompt text, quota
      values, screenshots, or local audit logs.
- [ ] Last upstream merge captured in commit history (see
      `docs/UPSTREAM_SYNC.md`).

```powershell
git status --short --branch
git log --oneline -10
```

## 2. Required Automated Gate

Copied from `docs/PRODUCTION_READINESS.md` — re-run on the exact commit being
shipped. All must pass.

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
cargo run -- --demo --task-evidence
cargo run -- --demo --dispatch-task dataset-drift-guardrails --dispatch-dry-run
```

Expected:

- formatting passes,
- clippy passes with warnings denied,
- all tests pass,
- build completes,
- demo exports do not include prompt text, file contents, or absolute demo
  paths,
- `--handoff --json` returns valid JSON with schema `abtop.agent_handoff.v1`.

## 3. Required Manual Gate

From `docs/PRODUCTION_READINESS.md` — must be performed against a real local
session, not the demo fixture.

- [ ] `cargo run -- --doctor` resolves all hard failures.
- [ ] `cargo run -- --setup` works on the target OS if Claude quota should be
      shown.
- [ ] At least one Claude Code or Codex session is running in a `.dw` project.
- [ ] In the TUI Workspace tab the selected `.dw` project shows: task status
      and phase, roadmap ready/blocked/stage counts, handoff lanes for
      `claude-code`/`codex-cli`/`opencode`, assignment rows with agent-fit
      hints, blocked/risky work under hold notes.
- [ ] `cargo run -- --handoff` and `cargo run -- --handoff --json` run end-to-end.
- [ ] Exported output contains **no** prompt text, file contents, secrets, or
      absolute local paths.
- [ ] Composer flow (skip when no `allow_dispatch_*` flag is set in
      `~/.config/abtop/config.toml`):
  - press `d` from the Workspace tab on a `.dw` task,
  - verify the brief preview redacts file contents and absolute paths,
  - with `ABTOP_DISPATCH_DRY_RUN=1` set, complete the double-Enter confirm
    and verify `outcome: dry-run` plus a fresh `dispatch-*` audit event,
  - with the env var unset (and the agent CLI on `PATH`), verify a
    response file is written to `{audit_dir}/dispatch/`, byte count > 0,
    and no prompt or file content is leaked in any output surface.

Capture the outcome in `docs/PRODUCTION_EVIDENCE.md` with today's date and
sanitized values only.

## 4. Documentation Cross-Check

- [ ] `README.md` install commands resolve on the target OS.
- [ ] `README.md` § *Agentic Workspace* walkthrough still matches actual demo
      output.
- [ ] `docs/LIMITATIONS.md` reflects current feature state (status heuristics,
      quota scope, OpenCode gaps).
- [ ] `docs/EXECUTION_BOARD.md` shows no `Doing` tasks owned by an agent that
      will not be present for the release.
- [ ] `docs/PRODUCTION_READINESS.md` checklist still matches automated +
      manual gates above.
- [ ] All committed docs are in English (`AGENTS.md` § *Language Policy*).

## 5. Version + Changelog

- [ ] `Cargo.toml` version bumped following semver: bug fixes → patch,
      additive features → minor, breaking CLI/output schema → major.
- [ ] `Cargo.lock` updated by running `cargo build`.
- [ ] Release notes drafted summarizing fork-only changes since previous
      release tag (Agentic Workspace surfaces, dw-kit reader, handoff, audit,
      policy gates, etc.).

```powershell
git log --oneline --no-merges <previous-tag>..HEAD
```

## 6. Tag + Distribution

The fork distribution path depends on how you ship:

### Option A — Contribute upstream

- [ ] Open PR against `graykode/abtop:main`.
- [ ] Reference relevant `docs/EXECUTION_BOARD.md` task IDs in the description.
- [ ] Defer the crates.io publish to the upstream maintainer (`publish.yml`
      runs on upstream tags only).

### Option B — Fork-only GitHub Release

- [ ] Tag on the fork repo: `git tag -a vX.Y.Z-fork.N -m "vX.Y.Z-fork.N"`.
- [ ] Push the tag: `git push origin vX.Y.Z-fork.N`.
- [ ] Watch fork-side workflows: `gh run list --workflow Release --limit 5`.
- [ ] Do **not** trigger the crates.io publish workflow from the fork — the
      `abtop` crate name is owned by upstream.
- [ ] Verify binaries on the fork's GitHub Releases page after `release.yml`
      finishes.

### Option C — Local-only (no public artifact)

- [ ] Build a local binary: `cargo build --release`.
- [ ] Document the install path in the team handoff note.

Pick **one** option per release and record it in
`docs/PRODUCTION_EVIDENCE.md`.

## 7. Post-Release

- [ ] Update `docs/EXECUTION_BOARD.md`: move any `Doing` GTM task to `Done`
      with EVD references to this release.
- [ ] Update `docs/PRODUCTION_EVIDENCE.md` with the release tag and the date
      the automated + manual gates last passed.
- [ ] Open the next milestone tasks if any were unlocked by this release
      (for example `P6-UX-01` becomes unblocked after `P4-DSP-01` ships).

## Do-Not Rules

- Do **not** skip `cargo fmt --check` or `clippy -D warnings` to ship faster.
- Do **not** reuse a release tag after a failed publish — bump to a new patch
  version instead (`AGENTS.md` rule).
- Do **not** push the tag before the version bump is on the source branch.
- Do **not** commit local audit logs, screenshots, or transcripts as release
  evidence — only sanitized counts and outcomes belong in
  `docs/PRODUCTION_EVIDENCE.md`.
- Do **not** force-push the release branch.
