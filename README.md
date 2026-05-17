# abtop

**Like [btop](https://github.com/aristocratos/btop), but for your AI coding agents.**

See every Claude Code, Codex CLI, and OpenCode session at a glance — token usage, context window %, rate limits, child processes, open ports, and more.
Claude Code, Codex CLI, and OpenCode sessions are discovered from local process/file state, so multiple active profiles are supported across macOS, Linux, and Windows.

![demo](https://raw.githubusercontent.com/graykode/abtop/main/assets/demo.gif)

## Why

- Running 3+ agents across projects? See them all in one screen.
- Hitting rate limits? Watch your quota in real-time.
- Agent spawned a server and forgot to kill it? Orphan port detection.
- Context window filling up? Per-session % bars with warnings.

Monitoring is read-only until you use an explicit control such as `x` or `X`.
No API keys. No auth.

## Install

### macOS / Linux

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/graykode/abtop/releases/latest/download/abtop-installer.sh | sh
```

### Cargo

```bash
cargo install abtop
```

### Windows

Native support — no WSL required. Uses `sysinfo` for process info and `netstat -ano` for listening ports.

```powershell
powershell -c "irm https://github.com/graykode/abtop/releases/latest/download/abtop-installer.ps1 | iex"
```

Or `cargo install abtop` from any terminal with Git in PATH. Claude Code config is resolved automatically from `%USERPROFILE%\.claude`.

### Other

Pre-built binaries for all platforms are available on the [GitHub Releases](https://github.com/graykode/abtop/releases) page.

## Usage

```bash
abtop                    # Launch TUI
abtop --once             # Print snapshot and exit
abtop --workspace-summary # Print redacted Workspace Markdown and exit
abtop --task-evidence    # Print redacted per-task evidence Markdown and exit
abtop --roadmap          # Print dependency-aware task roadmap Markdown and exit
abtop --handoff          # Print cross-agent assignment handoff Markdown and exit
abtop --handoff --json   # Print machine-readable handoff JSON and exit
abtop --setup            # Install rate limit collection hook
abtop --doctor           # Check local setup and collector health
abtop --doctor --json    # Print machine-readable diagnostics JSON
abtop --theme dracula    # Launch with a specific theme
```

Mutating controls require a second keypress within the confirmation window and
write append-only audit events to the local abtop data directory. Set
`ABTOP_AUDIT_FILE` to override the JSONL audit path, or set
`ABTOP_CONTROL_DRY_RUN=1` to audit verified controls without terminating
processes.
Workspace summaries include redacted `.dw` task counts and dependency-aware
roadmap sequencing so ready, blocked, and staged tasks can be reviewed before
assigning agents.
Handoff exports turn that roadmap into a safe shared workspace protocol for
Claude Code, Codex, OpenCode, or future local agents. Agents coordinate through
task state, dependency order, evidence, and blockers rather than an unaudited
private chat.
Use Markdown output for human planning and JSON output when another tool or
agent needs stable structured handoff context.

Recommended terminal size: **120x40** or larger. Minimum 80x24 — panels hide gracefully when small.

Quota bars show account rate-limit percentage remaining for each provider
window. They are separate from the session token totals shown in the same panel.

### tmux

abtop works standalone, but running inside tmux unlocks session jumping — press `Enter` to switch directly to the pane running that agent.

```bash
tmux new -s work
# pane 0: abtop
# pane 1: claude (project A)
# pane 2: claude (project B)
# → Enter on a session in abtop jumps to its pane
```

## Supported Agents

| Feature           | Claude Code | Codex CLI | OpenCode |
| ----------------- | :---------: | :-------: | :------: |
| Session Discovery |     ✅      |    ✅     |    ✅    |
| Token Tracking    |     ✅      |    ✅     |    ✅    |
| Context Window %  |     ✅      |    ✅     |    ❌    |
| Status Detection  |     ✅      |    ✅     |    ✅    |
| Current Task      |     ✅      |    ✅     |    ❌    |
| Rate Limit        |     ✅      |    ✅     |    ❌    |
| Git Status        |     ✅      |    ✅     |    ✅    |
| Children / Ports  |     ✅      |    ✅     |    ✅    |
| Subagents         |     ✅      |    ❌     |    ❌    |
| Memory Status     |     ✅      |    ❌     |    ❌    |

OpenCode support reads the local SQLite database at `~/.local/share/opencode/opencode.db` and requires `sqlite3` in `PATH`.

## Diagnostics

Run `abtop --doctor` when session discovery, quota display, OpenCode support, tmux jumping, process termination, or port detection does not look right. It checks local-only dependencies and collector prerequisites without reading transcript contents. Use `abtop --doctor --json` in scripts or bug reports; warning-only results exit successfully, while hard collector failures exit non-zero.

## Themes

12 built-in themes, including 4 colorblind-friendly options (`high-contrast`, `protanopia`, `deuteranopia`, `tritanopia`). Press `t` to cycle at runtime, or launch with `--theme <name>`. Your choice is saved to `~/.config/abtop/config.toml`.

| btop (default) | dracula | catppuccin |
|:-:|:-:|:-:|
| ![btop](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/btop.png) | ![dracula](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/dracula.png) | ![catppuccin](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/catppuccin.png) |

| tokyo-night | gruvbox | nord |
|:-:|:-:|:-:|
| ![tokyo-night](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/tokyo-night.png) | ![gruvbox](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/gruvbox.png) | ![nord](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/nord.png) |

Colorblind-friendly themes:

| high-contrast | protanopia |
|:-:|:-:|
| ![high-contrast](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/high-contrast.png) | ![protanopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/protanopia.png) |

| deuteranopia | tritanopia |
|:-:|:-:|
| ![deuteranopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/deuteranopia.png) | ![tritanopia](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/tritanopia.png) |

Light themes (`light` — Solarized cream, `white` — GitHub-style pure white) for bright terminals:

| light | white |
|:-:|:-:|
| ![light](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/light.png) | ![white](https://raw.githubusercontent.com/graykode/abtop/main/assets/themes/white.png) |

## Configuration

`~/.config/abtop/config.toml` supports:

```toml
theme = "btop"
# Hide specific agent CLIs from the TUI (case-insensitive).
# Useful if you only use one agent and want a cleaner view.
hidden_agents = ["codex"]
# UI language. English is the supported project-facing language.
language = "en"
# Local policy gates for mutating controls.
allow_kill_sessions = true
allow_kill_orphan_ports = true
```

`language` is kept for config-file compatibility. English is currently the supported UI language.

## Key Bindings

| Key                | Action                               |
| ------------------ | ------------------------------------ |
| `↑`/`↓` or `k`/`j` | Select session                       |
| `Enter`            | Jump to session terminal (tmux only) |
| `x`                | Confirm kill selected session        |
| `X`                | Confirm kill all orphan ports        |
| `t`                | Cycle theme                          |
| `1`–`5`            | Toggle panel visibility              |
| `Esc`              | Open/close config page               |
| `q`                | Quit                                 |
| `r`                | Force refresh                        |

## Privacy

abtop reads local files and local process/open-file metadata only. No API keys, no auth. Tool names and file paths are shown in the UI, but file contents are never displayed. Session summaries are generated via `claude --print`, which makes its own API call — this is the only indirect network usage. Prompt text is sanitized and secret-redacted before summary generation, and local fallback titles do not include prompt text. Set `ABTOP_DISABLE_SUMMARIES=1` to skip summary generation and show generic local titles.

## Acknowledgements

Huge thanks to [@tbouquet](https://github.com/tbouquet) for driving much of abtop's recent shape — themes, config overlay and panel toggles, session filtering, subagent tree view, the context window gauge with compaction detection, plus a steady stream of fixes and security hardening along the way.

## License

MIT
