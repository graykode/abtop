# Fork Security Scan and Quick Deployment (abtop)

Date: 2026-05-14 (UTC)

## Scope

This report covers a fast security-oriented validation and deployment checklist for this fork of `graykode/abtop`.

## What was checked

1. **Code quality and baseline safety checks**
   - `cargo fmt -- --check`
   - `cargo clippy --all-targets --all-features -- -D warnings`
   - `cargo test`

2. **Dependency vulnerability scan intent**
   - Attempted to run `cargo audit`.
   - Attempted to install `cargo-audit` when not present.

## Results

- `cargo fmt -- --check` failed initially due to formatting drift in tracked files.
  - No functional/security bug indicated, but formatting should be normalized before release.
- `cargo clippy --all-targets --all-features -- -D warnings` passed.
- `cargo test` passed (139/139 tests).
- `cargo audit` could not be executed because `cargo-audit` is not installed.
- `cargo install cargo-audit` failed in this environment due to crates.io network/proxy denial (`CONNECT tunnel failed, response 403`).

## Security observations (quick)

Based on project docs and architecture:

- The app is local-first and read-only for session discovery, reducing remote attack surface.
- It reads sensitive local artifacts (transcripts/memory). Operators should:
  - run with least-privileged user,
  - lock down `~/.claude`, `~/.codex`, and shell history permissions,
  - avoid sharing screenshots from real sessions.
- Input parsing is schema-drift tolerant and defensive (`serde(default)` strategy documented), which helps availability against malformed local data.
- Runtime process/port introspection (`ps`, `lsof`, `sqlite3`) should be treated as privileged telemetry; in shared hosts, run only in trusted user contexts.

## Quick deploy for this fork

### Option A: Local release binary

```bash
cargo build --release
./target/release/abtop --once
./target/release/abtop --setup
```

### Option B: Install to user cargo bin

```bash
cargo install --path .
abtop --once
abtop --setup
```

### Option C: CI release flow (recommended for fork maintainers)

1. Bump `Cargo.toml` + `Cargo.lock` version.
2. Run:
   - `cargo test`
   - `cargo clippy -- -D warnings`
   - `cargo build --release`
3. Commit version bump.
4. Push to `main`.
5. Create annotated tag `vX.Y.Z` and push.
6. Let release workflows publish binaries/crates artifacts.

## Next actions recommended

1. Re-run vulnerability audit in an environment with crates.io access:
   - `cargo install cargo-audit`
   - `cargo audit`
2. Optionally add `cargo audit`/`cargo deny` to CI for scheduled dependency checks.
3. Normalize formatting drift (`cargo fmt`) before next release commit.
