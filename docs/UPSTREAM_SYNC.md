# Upstream Sync Guide

This fork should keep a clean path back to `graykode/abtop` while still moving
forward with fork-specific Agentic Workspace work.

## Remote Model

Expected remotes:

```powershell
git remote -v
```

```text
origin   https://github.com/huygdv/abtop.git
upstream https://github.com/graykode/abtop.git
```

Safety rule:

- `origin` is writable.
- `upstream` is read-only.
- The local upstream push URL should stay disabled.

Verify:

```powershell
git remote get-url --push upstream
```

Expected output:

```text
DISABLED
```

If it is not disabled, run:

```powershell
git remote set-url --push upstream DISABLED
```

## Branch Policy

- `main`: keep close to upstream.
- `codex/agentic-workspace-mvp`: product branch for the fork.
- `codex/<short-feature>`: small feature branches when work needs isolation.

Avoid long-running work directly on `main`.

## Sync Cadence

Check upstream:

- before starting a major milestone,
- before a fork release,
- when upstream ships a bugfix or agent-support feature,
- when upstream changes files we are actively modifying,
- at least weekly while the fork is active.

Do not merge every upstream commit immediately if the fork is in the middle of a
risky product slice. Prefer syncing at clean checkpoints.

## Standard Sync Flow

1. Make sure the worktree is clean:

   ```powershell
   git status --short --branch
   ```

2. Update upstream refs:

   ```powershell
   git fetch upstream
   ```

3. Update fork `main`:

   ```powershell
   git checkout main
   git merge --ff-only upstream/main
   git push origin main
   ```

   If fast-forward fails, inspect history before deciding whether to merge.

4. Bring the product branch up to date:

   ```powershell
   git checkout codex/agentic-workspace-mvp
   git merge main
   ```

5. Run gates:

   ```powershell
   cargo fmt -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test
   cargo run -- --demo --once
   cargo run -- --demo --workspace-summary
   ```

6. Push the product branch:

   ```powershell
   git push origin codex/agentic-workspace-mvp
   ```

## Merge vs Rebase

Use merge by default for the product branch:

```powershell
git checkout codex/agentic-workspace-mvp
git merge main
```

Reasons:

- preserves the branch history that has already been pushed,
- avoids rewriting shared history,
- makes upstream sync points visible.

Use rebase only on local, unpublished short feature branches.

## Cherry-Pick Option

If upstream ships one urgent fix but a full sync is risky, cherry-pick it onto
the product branch:

```powershell
git fetch upstream
git checkout codex/agentic-workspace-mvp
git cherry-pick <upstream_commit_sha>
```

After cherry-picking, record why a full merge was skipped in the commit or PR
notes.

## Conflict Hotspots

Expect conflicts in:

- `src/app.rs`,
- `src/ui/*`,
- `src/collector/*`,
- `src/setup.rs`,
- `README.md`,
- roadmap and development docs.

Conflict strategy:

- preserve upstream bugfixes unless they clearly regress fork behavior,
- preserve fork privacy boundaries and Windows support,
- avoid deleting Agentic Workspace docs or tracker state,
- prefer extracting fork-specific code into smaller modules over expanding
  already-conflicted files.

## Post-Conflict Checklist

After resolving conflicts:

```powershell
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo run -- --demo --once
cargo run -- --demo --workspace-summary
```

For Windows-sensitive changes, also verify:

```powershell
abtop --setup
Get-Content $HOME\.claude\abtop-rate-limits.json
netstat -ano -p TCP | Select-String -Pattern 'LISTENING' | Select-Object -First 8
```

## Upstream Contribution Rule

If a change is broadly useful and not fork-product-specific, consider preparing
a clean upstream PR.

Good upstream candidates:

- bugfixes,
- Windows compatibility,
- parser robustness,
- privacy-safe diagnostics,
- small UI clarity improvements.

Keep fork-specific strategy, dw-kit integration, and Agentic Workspace product
direction in the fork unless upstream explicitly wants them.

## Release Sync Rule

Before any fork release:

1. Sync or consciously defer upstream.
2. Run all gates.
3. Update release notes with upstream base commit and fork commits.
4. Tag only from a clean, pushed branch.
