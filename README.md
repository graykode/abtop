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

The Agentic Workspace surfaces (`--workspace-summary`, `--roadmap`, `--handoff`,
`--task-evidence`, plus the in-TUI Workspace tab) are documented under
[Agentic Workspace](#agentic-workspace) below. Production readiness checks live
in [`docs/PRODUCTION_READINESS.md`](docs/PRODUCTION_READINESS.md). Known
limitations are listed in [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md).

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

## Agentic Workspace

When a project uses [dw-kit](https://github.com/dv-workflow/dw-kit) — or any
layout that mirrors `.dw/tasks/`, `.dw/decisions/`, `.dw/records/` — abtop
reads task state alongside live agent sessions. The same screen shows what is
running, which task it belongs to, what is ready next, and what is blocked.

Two or more agents (for example Claude Code and Codex) running against the
**same project directory** are merged into one Workspace project. They
coordinate through the shared task graph + evidence — not through a private
agent-to-agent chat. See `docs/PRODUCT_STRATEGY.md` for the rationale.

### Prerequisites

- a `.dw/tasks/` directory in the project (v2 `spec.md`/`tracking.md` or legacy
  3-file layout — both parsed),
- at least one running Claude Code, Codex CLI, or OpenCode session inside that
  directory.

### Walkthrough

Open the TUI and press `a` to focus the Workspace tab:

```bash
abtop
# press `a` to focus Workspace
```

The Workspace tab shows: active task title, phase, acceptance/verification
counts, roadmap stages (ready/blocked/next), handoff lanes per agent, and
assignment suggestions.

For headless / scripted use, four redacted exports cover the same data:

```bash
abtop --workspace-summary    # Markdown — what is running, per project
abtop --roadmap              # Markdown — ready/blocked/staged tasks + risks
abtop --handoff              # Markdown — task assignment queue per agent
abtop --handoff --json       # JSON  — schema: abtop.agent_handoff.v1
abtop --task-evidence        # Markdown — per-task counts, tools, files, risks
```

Example (`--demo --handoff`, abbreviated):

```text
# abtop agent handoff

- coordination: shared workspace protocol
- handoff lanes: claude-code, codex-cli, opencode
- privacy: redacted task metadata only; no prompt text or file contents

## ml-pipeline
- active agents: claude
- ready now: 2
- blocked: 1
- assignment queue:
  - first stage 1: Dataset drift guardrails [Ready]
    suggested agent: implementation agent
    evidence: deps=0 verification=0/1
  - next stage 2: Model card refresh [Review]
    suggested agent: second agent reviewer
- do not assign yet:
  - blocked task: Production access approval
- live coordination notes:
  - claude wait: waiting for user input
```

Run any export with `--demo` to see synthetic data without a real session.

### Output Guarantees

All five Agentic Workspace surfaces:

- contain task titles, statuses, counts, and redacted tool labels only,
- never include prompt text, file contents, transcript bodies, or absolute
  local paths,
- can be piped into another tool or shared with a reviewer without further
  scrubbing.

`--handoff --json` carries a stable schema (`abtop.agent_handoff.v1`) suitable
as startup context for another agent.

### Roadmap of this surface

What is **in scope today**: read-only task/runtime view, redacted exports,
audited destructive controls (`kill session`, `kill orphan port`).

What is **deliberately deferred** (per `docs/AGENT_HANDOFF.md`):

- automatic task dispatch and reply,
- direct agent-to-agent private chat in abtop,
- cloud/team sync.

These can only be added after the policy + audit + redaction model is
extended — see `P4-DSP-01` and `P6-UX-01` in `docs/EXECUTION_BOARD.md`.

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
