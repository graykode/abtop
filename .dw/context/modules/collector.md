# Module: collector

## Vai trò

Read-only data acquisition layer. Discovers live agent sessions (Claude Code, Codex CLI, OpenCode), parses transcripts/rollouts, walks the process tree, reads open ports, and reads StatusLine-derived rate limits. All data sources are undocumented internals of the agent CLIs — defensive parsing is mandatory.

## Files chính

| File | Vai trò |
|------|---------|
| `claude.rs` (~162KB) | Claude Code: process+config-root mapping, `sessions/{PID}.json`, transcript JSONL incremental tail (offset tracking, partial-line buffering, file-rotation reset) |
| `codex.rs` (~62KB) | Codex CLI: process+`lsof` → `rollout-*.jsonl`, parses `session_meta` / `token_count` (incl. `rate_limits`) / `agent_message` events |
| `opencode.rs` (~21KB) | OpenCode: process discovery + read recent sessions from `~/.local/share/opencode/opencode.db` via `sqlite3 -readonly -json`. cwd-based PID→session matching |
| `process.rs` (~25KB) | `ps`/`pgrep` child-process tree, `lsof`/`netstat` listening ports, git `status --porcelain` per project. Windows netstat parser handles IPv4/IPv6/duplicate rows |
| `rate_limit.rs` (~5KB) | Read `~/.claude/abtop-rate-limits.json` (StatusLine-fed). Rejects data > 10 min old |
| `mcp.rs` (~13KB) | MCP server inventory |
| `mod.rs` (~15KB) | `MultiCollector` orchestrator, `redact_secrets()`, `sanitize_terminal_text()`, re-exports |

## Public API / Exports

- `MultiCollector` — top-level orchestrator polled by `App` tick
- `ClaudeCollector`, `CodexCollector`, `OpenCodeCollector` — per-agent collectors
- `McpServer` — MCP server data type
- `read_rate_limits()` — pure function reading StatusLine output
- `redact_secrets(s)` — strip well-known token prefixes (sk-ant-, sk-proj-, ghp_, AKIA, Bearer …) with `[REDACTED]`
- `sanitize_terminal_text(s)` — strip control sequences for safe rendering

## Dependencies

- **Upstream (depends on)**: `model` (`AgentSession`, `FileAccess`, `ToolCall`, `RateLimitInfo`, `OrphanPort`, `SessionStatus`)
- **Downstream (used by)**: `app.rs` (primary consumer), indirectly `evidence`, `roadmap`, `ui`
- **External**: `ps`, `lsof`, `netstat`, `sqlite3`, `git`, filesystem (`~/.claude/`, `~/.codex/`, `~/.local/share/opencode/`)

## Conventions riêng

- **`serde(default)` everywhere** — all upstream schemas may change without notice (Claude Code, Codex, OpenCode internals).
- **Incremental file reads**: large transcripts (1KB–18MB) are tailed by tracking byte offset; partial lines buffered until the next read; offset reset on file shrinkage (session restart).
- **Path encoding** for Claude transcripts: `/Users/foo/bar` → `-Users-foo-bar`. Collision possible (e.g. `-Users-foo-bar-baz`) — disambiguate using `sessions/{PID}.json` `cwd` as source of truth.
- **PID liveness check** uses `ps -p {pid} -o command=` to defend against PID reuse — don't trust PID alone.
- **Hidden agents**: `MultiCollector` honors `AppConfig.hidden_agents` (case-insensitive `agent_cli` match).
- **Cache slow calls**: `lsof` is slow on macOS with many open files — polled every 10s (5 ticks), not every tick.

## Lưu ý cho AI

- **Privacy contract**: collectors hold raw prompts, tool inputs, and transcript content. They MUST pass content through `redact_secrets` + `sanitize_terminal_text` before any data leaves the collector boundary toward UI/export modules. Adding a new field that exposes raw text to `App` is a regression.
- **Context window calculation**: NOT in transcripts. Hardcoded per model in collector → `claude-opus-4-6` = 200k, `claude-opus-4-6[1m]` = 1M, sonnet/haiku-4-x = 200k. Current usage = last assistant's `input_tokens + cache_read_input_tokens` only (excluding `cache_creation_input_tokens` to avoid double-counting on compaction turns — see issue #54).
- **Session deletion race**: Claude `sessions/{PID}.json` is removed on normal exit; handle `NotFound` between scan and read gracefully.
- **`/clear` ambiguity**: after `/clear`, Claude Code mints a new sessionId but does NOT rewrite `sessions/{PID}.json`. abtop overrides with newest transcript in project dir; this heuristic is disabled when two live `claude` PIDs share a cwd.
- **Subagent directory** (`{config-root}/projects/{path}/{sessionId}/subagents/`) may not exist — created only on Agent tool use. Check existence first.
- **Port detection race**: a port can close between `lsof` and display. Show stale data gracefully; don't panic.
- **Windows TCP parsing**: `parse_windows_netstat_ports_*` tests exist in `process.rs` — keep them passing when touching that logic (commit `d10f5aa`).
- **Quota scope**: Claude + Codex only. OpenCode does NOT contribute quota data — don't add an OpenCode quota row.
