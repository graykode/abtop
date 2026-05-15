# Development Guide

This fork is developed primarily on native Windows while keeping upstream
compatibility with macOS and Linux.

## Repository Model

- `origin`: fork owned by `huygdv`.
- `upstream`: original `graykode/abtop` repository.
- `upstream` push URL is intentionally disabled locally to avoid accidental
  pushes to the original project.

Use feature branches for all work:

```powershell
git checkout main
git fetch upstream
git merge upstream/main
git checkout -b codex/<short-feature-name>
```

Push feature branches to the fork:

```powershell
git push -u origin codex/<short-feature-name>
```

Detailed upstream merge, cherry-pick, and conflict-resolution rules live in
`docs/UPSTREAM_SYNC.md`.

## Local Windows Toolchain

Required:

- Git
- Rustup with `stable-x86_64-pc-windows-msvc`
- Visual Studio Build Tools 2022 with C++ build tools
- Cargo bin in `PATH`: `%USERPROFILE%\.cargo\bin`

In a fresh PowerShell session, Cargo may be available only after adding:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
```

If Cargo cannot find `link.exe`, run cargo commands through the Visual Studio
developer environment:

```powershell
cmd /c "call ""C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat"" -arch=x64 -host_arch=x64 >nul && set PATH=%USERPROFILE%\.cargo\bin;%PATH% && cargo test"
```

## Quality Gates

Run these before pushing behavior changes:

```powershell
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
abtop --once
```

For dependency review:

```powershell
cargo audit
```

Current known audit warnings are documented in
`docs/WINDOWS_LOCAL_DEV_DOD.md`.

## Runtime Checks

Snapshot mode verifies collector output without opening the TUI:

```powershell
abtop --once
abtop --demo --once
abtop --demo --workspace-summary
```

Quota panel semantics:

- Claude and Codex quota bars show account rate-limit percentage remaining for
  the 5-hour and 7-day windows.
- The displayed percentage is `100 - used_percent` from the provider telemetry;
  it is not an absolute token count.
- The `total` row is session token telemetry collected by abtop and should not
  be compared directly with Settings pages that show rate-limit remaining.
- Codex values come from the latest Codex CLI `token_count` rate-limit event or
  the cached fallback at `%LOCALAPPDATA%\abtop\codex-rate-limits.json`.
- Claude values come from `%USERPROFILE%\.claude\abtop-rate-limits.json`,
  written by the Claude Code StatusLine hook installed by `abtop --setup`.

Interactive mode verifies the full realtime UI:

```powershell
abtop
```

Use Windows Terminal or another real interactive terminal. Recommended size is
120x40 or larger.

In compact/narrow layouts, press `a` to open the Agentic Workspace tab. It
shows a read-only project rollup with live agent counts, context pressure,
tokens, git changes, ports, and `.dw` workflow hints.

In desktop layouts, the same `a` shortcut opens the Agentic Workspace focus
view. This is covered by the `desktop_workspace_focus_renders_workspace_panel`
test.

Workspace verification:

```powershell
cargo test workspace
cargo run -- --demo
```

After the demo TUI opens, press `a`. The view should switch to the Workspace
project rollup. In Workspace focus, use `j/k` or arrow keys to move between
projects, then press `Enter` to select that project's first session in the
sessions panel. Press `a` again to return to the regular desktop view.

Workspace GIF evidence can be regenerated with Charm VHS:

```powershell
cargo build
cd assets
vhs workspace-demo.tape
```

This writes `assets/workspace-demo.gif`.

On native Windows, install the recorder dependencies with:

```powershell
winget install --id charmbracelet.vhs --exact
winget install --id tsl0922.ttyd --exact
```

If the local VHS/ttyd path hangs in a Windows terminal, use the Docker path that
matches the repository tape:

```powershell
docker run --rm -v "${PWD}:/work" -w /work rust:1-bookworm cargo build
docker run --rm -v "${PWD}:/vhs" -w /vhs/assets ghcr.io/charmbracelet/vhs workspace-demo.tape
```

The Docker path builds the Linux demo binary used by the tape and then renders
`assets/workspace-demo.gif`.

## Diagnostics Logging

Runtime logging is file-based so it does not corrupt the TUI screen. Logging is
disabled by default.

Enable default log file:

```powershell
$env:ABTOP_LOG = "1"
$env:ABTOP_LOG_LEVEL = "debug"
abtop --once
```

Default log location:

```text
%LOCALAPPDATA%\abtop\abtop.log
```

Use a specific log file:

```powershell
$env:ABTOP_LOG_FILE = "$PWD\abtop-debug.log"
$env:ABTOP_LOG_LEVEL = "trace"
abtop --once
```

Levels: `error`, `warn`, `info`, `debug`, `trace`.

Guidelines:

- Keep logs operational: counts, modes, errors, timings, and state transitions.
- Do not log prompt text, file contents, transcript lines, auth tokens, or full
  tool inputs.
- Prefer session IDs and aggregate counts over raw user content.
- In TUI mode, never write diagnostic output to stdout or stderr.

## Baseline Architecture

Core loop:

- TUI redraw: every 500 ms.
- Agent data refresh: every 2 seconds.
- Slow process, port, git, and rate-limit refresh: every 10 seconds.

Main areas:

- `src/main.rs`: CLI flags, terminal lifecycle, event loop.
- `src/app.rs`: app state, tick logic, selection, summaries.
- `src/collector/`: Claude, Codex, OpenCode, MCP, process and rate-limit data.
- `src/diagnostics.rs`: optional file-based logging for development/debugging.
- `src/ui/`: panel rendering and interaction surfaces.
- `src/model/`: shared session and rate-limit structures.
- `src/setup.rs`: Claude StatusLine hook installer.

## Windows Notes

The main monitor path works on native Windows. The `--setup` path now installs
a PowerShell StatusLine hook at `%USERPROFILE%\.claude\abtop-statusline.ps1`
or the equivalent `CLAUDE_CONFIG_DIR` path. The generated Claude command uses:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "<path>\abtop-statusline.ps1"
```

Run setup with:

```powershell
abtop --setup
```

Then restart Claude Code and send one message before expecting Claude
rate-limit telemetry in abtop. If a custom `statusLine` command already exists,
setup refuses to overwrite it; remove or merge that key manually before running
setup again.
