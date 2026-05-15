# Windows Local Dev Deployment DoD

Date: 2026-05-14
Host: Windows native, x86_64-pc-windows-msvc

## Outcome

This repository is set up for native Windows local development and deployment.
The `abtop` binary has been built from this workspace, installed into the
current user's Cargo bin directory, and smoke-tested against the local machine.

## Installed Prerequisites

- Git is available from `C:\Program Files\Git\cmd\git.exe`.
- Rustup is installed through `winget`.
- Active Rust toolchain: `stable-x86_64-pc-windows-msvc`.
- Rust version verified: `rustc 1.95.0`.
- Cargo version verified: `cargo 1.95.0`.
- Visual Studio Build Tools 2022 with C++ build tools is installed for
  `link.exe` and the MSVC linker environment.
- `cargo-audit 0.22.1` is installed in `C:\Users\APC\.cargo\bin`.

## Deployment Result

- Release binary build succeeded:
  `cargo build --release`
- User-local install succeeded:
  `cargo install --path . --force`
- Installed executable:
  `C:\Users\APC\.cargo\bin\abtop.exe`
- Installed version:
  `abtop 0.4.4`
- Smoke test succeeded:
  `abtop --once`
- Smoke test observed local runtime data:
  `abtop - 5 sessions, 0 mcp servers`
- Repeated live snapshots succeeded:
  `abtop --once` was run twice with a 3 second gap and continued to read
  current Codex/Claude session state from the local Windows host.
- Demo data snapshot succeeded:
  `abtop --demo --once` rendered the same class of panels shown in the README
  demo: context, quota, tokens, projects, ports, sessions, children, and status.

## Realtime UI Baseline

The interactive TUI path is wired for realtime operation:

- TUI render loop redraws every 500 ms.
- Live collector refreshes agent data every 2 seconds.
- Slower process/port/git/rate-limit work refreshes every 10 seconds.
- Manual refresh is available with `r`.
- Demo mode animates token-rate sparkline data without touching live sessions.

The installed binary has been smoke-tested in snapshot mode against real local
session data. For a visual realtime check, run `abtop` in a normal interactive
terminal with at least an 80x24 window; 120x40 or larger is recommended.

## Verification Gates

- Formatting:
  `cargo fmt -- --check` passed.
- Lint:
  `cargo clippy --all-targets --all-features -- -D warnings` passed.
- Tests:
  `cargo test` passed with `137 passed; 0 failed`.
- Release build:
  `cargo build --release` passed.
- Dependency audit:
  `cargo audit` completed without failing vulnerabilities.

## Audit Notes

`cargo audit` reported two warnings from transitive dependencies under
`ratatui 0.29.0`:

- `RUSTSEC-2024-0436`: `paste 1.0.15` is unmaintained.
- `RUSTSEC-2026-0002`: `lru 0.12.5` has an unsound `IterMut` advisory.

These are warnings in the current dependency tree, not deployment blockers for
this local install. A future dependency refresh should evaluate upgrading
`ratatui` and related transitive dependencies.

## Windows Fix Applied

The Claude collector test helper now serializes session JSON with `serde_json`
instead of string interpolation. This preserves Windows paths correctly by
escaping backslashes in `cwd` and keeps the Windows test suite green.

Windows TCP port detection now parses `netstat -ano -p TCP` by explicit columns
instead of treating the first token as the local address. Tests cover IPv4,
IPv6, duplicate dual-stack rows, and non-listening rows.
