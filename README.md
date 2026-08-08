# abtop — Factory Droid edition

**Like [btop](https://github.com/aristocratos/btop), but for your AI coding agents.**

Monitor Claude Code, Codex CLI, OpenCode, and **Factory Droid** sessions at a glance — token usage, context window %, rate limits, child processes, open ports, and more. All read-only, from local files and processes. No API keys, no auth.

> **Fork notice.** This is a fork of [graykode/abtop](https://github.com/graykode/abtop). Everything upstream is preserved; the additions live under [What's different](#whats-different).

![demo](https://raw.githubusercontent.com/graykode/abtop/main/assets/demo.gif)

## Features

- Single-screen monitoring for **Claude Code**, **Codex CLI**, **OpenCode**, and **Factory Droid**.
- **Factory Droid**: live sessions + worker subagents, custom-model catalog, missions, and config validation — read from `~/.factory`.
- **Accurate token tracking for Factory Droid**: usage is parsed from `sessions/**/<sessionId>.settings.json` → `tokenUsage`; worker tokens are attributed via the parent's `childInclusiveTokenUsageBySessionId`.
- Real-time rate limits, context window %, per-session token rate, orphan port detection.
- 12 built-in themes (4 colorblind-friendly), EN/ZH UI, runtime config overlay.

## What's different

Main change: a **Factory Droid collector** (`src/collector/factory.rs`) plus two new panels.

| Area | Upstream | This fork |
|---|---|---|
| Factory Droid | — | ✅ sessions, models, missions, config validation |
| Token usage (Factory Droid) | — | ✅ from `*.settings.json` → `tokenUsage` |
| Worker subagent tokens | — | ✅ from `childInclusiveTokenUsageBySessionId` |
| Models / missions panels | — | ✅ toggle with `8` / `9` |
| JSON snapshot | — | `factory` block (models, missions, issues) |

**Token fix** (commit `637dcb1`): Factory Droid does not write token usage into session `.jsonl` logs — it lives in `sessions/**/<sessionId>.settings.json`. The collector now scans and parses those files, which populates the Tokens column, the tokens panel, footer totals, and the token rate.

## Install / Build

The fork ships as source. Build it yourself:

```bash
# Build from source (Windows / macOS / Linux — native, no WSL required)
cargo build --release

# Install from the local checkout
cargo install --path .
```

Pre-built binaries: upstream [releases](https://github.com/graykode/abtop/releases).

## Usage

```bash
abtop                    # Launch TUI
abtop --once             # Print snapshot and exit
abtop --json             # One JSON snapshot and exit (for scripts/tools)
abtop --status-json      # Compact status JSON without local paths/prompts
abtop --setup            # Install rate limit collection hook
abtop --theme dracula    # Launch with a specific theme
```

Recommended terminal size: **120x40**. Minimum 80x24 — panels degrade gracefully.

Factory Droid models/missions panels are off by default. Toggle at runtime with `8` / `9`, or enable in `~/.config/abtop/config.toml`:

```toml
show_models = true
show_missions = true
```

`hidden_agents = ["factory"]` disables the Factory Droid collector entirely.

## Supported Agents

| Feature           | Claude Code | Codex CLI | OpenCode | Factory Droid |
| ----------------- | :---------: | :-------: | :------: | :-----------: |
| Session Discovery |      ✅      |    ✅     |    ✅    |       ✅      |
| Token Tracking    |      ✅      |    ✅     |    ✅    |       ✅      |
| Context Window %  |      ✅      |    ✅     |    ❌    |       ❌      |
| Status Detection  |      ✅      |    ✅     |    ✅    |       ✅      |
| Current Task      |      ✅      |    ✅     |    ❌    |       ✅      |
| Rate Limit        |      ✅      |    ✅     |    ❌    |       ❌      |
| Git Status        |      ✅      |    ✅     |    ✅    |       ❌      |
| Children / Ports  |      ✅      |    ✅     |    ✅    |       ❌      |
| Subagents         |      ✅      |    ❌     |    ❌    |       ✅      |
| Memory Status     |      ✅      |    ❌     |    ❌    |       ❌      |

Factory Droid support reads `~/.factory`: live sessions from `sessions-index.json` (orchestrator plus worker subagents), token usage from `sessions/**/<sessionId>.settings.json`, the custom-model catalog from `settings.json` / `factory-settings.json`, missions from `missions/<id>/`, and a config validator that flags duplicate ids, dangling default model references, and stale state files. API keys are never read.

## Configuration

`~/.config/abtop/config.toml`:

```toml
theme = "btop"
hidden_agents = ["codex"]
show_models = true
show_missions = true
claude_config_dirs = ["~/.claude-personal", "~/.claude-work-team"]
language = "zh"
```

UI language: `en` (default) or `zh`; auto-detected from `LANG` when unset.

## Key Bindings

| Key                | Action                               |
| ------------------ | ------------------------------------ |
| `↑`/`↓` or `k`/`j` | Select session                       |
| `Enter`            | Jump to session terminal (tmux only) |
| `x`                | Kill selected session                |
| `X`                | Kill all orphan ports                |
| `t`                | Cycle theme                          |
| `1`–`9`            | Toggle panel visibility              |
| `Esc`              | Open/close config page               |
| `q`                | Quit                                 |
| `r`                | Force refresh                        |

## Library / JSON snapshot

abtop is also a library crate, so local tools can reuse its data-collection layer in-process and serialize the same state the TUI renders:

```bash
abtop --json          # one-shot JSON snapshot for scripts
abtop --status-json   # compact status summary; omits paths, prompts, session ids
```

For long-running consumers, build an `App`, refresh with `App::tick_no_summaries()` (never spawns `claude --print`), and call `App::to_snapshot(interval_ms)`:

```rust,no_run
use abtop::app::App;
use abtop::{config, theme::Theme};

let cfg = config::load_config();
let mut app = App::new_with_config_and_claude_dirs(
    Theme::default(), &cfg.hidden_agents, cfg.panels, &cfg.claude_config_dirs,
);
app.tick_no_summaries();
let json = serde_json::to_string(&app.to_snapshot(2_000)).unwrap();
```

## Privacy

abtop reads local files and local process/open-file metadata only. No API keys, no auth. Tool names and file paths are shown, but file contents and prompt text are never displayed. The full `--json` snapshot includes local dashboard data (summaries, chat text, working directories, tool-call previews, token counts) plus a `factory` block — treat it as private and don't expose it without your own access controls. `--status-json` emits only aggregate health/quota fields.

## License

MIT
