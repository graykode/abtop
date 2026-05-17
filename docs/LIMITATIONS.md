# Known Limitations

abtop reads undocumented internals of Claude Code, Codex CLI, and OpenCode.
Most heuristics are best-effort. This page lists the limits a user should know
before trusting a number on screen.

For architectural detail behind each item, see `AGENTS.md`.

## Session Status Is Heuristic

The status indicator (Working / Waiting / Error / Done) is derived from PID
liveness plus transcript modification time, not from a structured signal the
CLI emits.

- `Working` = PID alive + transcript mtime < 30s ago
- `Waiting` = PID alive + transcript mtime > 30s ago
- `Error`   = PID alive + last assistant message contains error content
- `Done`    = PID has exited

Consequences:

- A long-running tool call (cargo build, npm test, docker pull) can show as
  `Waiting`.
- abtop cannot distinguish model-thinking, tool-executing, rate-limit waiting,
  and permission-prompt waiting from each other.
- Status is not authoritative — do not script on it for safety-critical
  decisions.

## PID Reuse

A PID can be reused by the OS once the original process exits. abtop verifies
each tracked PID against `ps -p {pid} -o command=` (or the platform-specific
equivalent) on every refresh, but a stale snapshot can survive for one tick.

Workaround: trust the running PID list, not a saved one.

## Claude Code `/clear` Ambiguity

When a user runs `/clear` in Claude Code, the CLI mints a new `sessionId` and
new transcript file, but does **not** rewrite `~/.claude/sessions/{PID}.json`.
abtop normally promotes the newest transcript in the project directory.

When **two** live `claude` PIDs share the same `cwd`, the promotion is
ambiguous and is disabled — both sessions keep their original sessionId until
exit. Use separate worktrees if live tracking of both is needed.

## Quota Coverage

The Quota panel shows Claude + Codex rate-limit gauges only.

- Claude rate limits are collected by a StatusLine hook installed by
  `abtop --setup`. Pro/Max account-level metric, shared across sessions.
- Codex rate limits are extracted from the `token_count` event in
  `rollout-*.jsonl`.
- OpenCode does **not** expose account-level rate limits. No OpenCode quota
  row is shown by design.

Stale data older than 10 minutes is rejected (shown as `—`).

## Context Window Hardcoded Per Model

Context window size is not present in any data source. abtop hardcodes it per
model name:

| Model                       | Window      |
|-----------------------------|-------------|
| `claude-opus-4-6`           | 200,000     |
| `claude-opus-4-6[1m]`       | 1,000,000   |
| `claude-sonnet-4-6`         | 200,000     |
| `claude-haiku-4-5`          | 200,000     |
| `claude-opus-4-7`           | 200,000     |
| `claude-opus-4-7[1m]`       | 1,000,000   |
| Codex / OpenAI models       | per upstream metadata |

New models added by Anthropic/OpenAI will display an unknown window until
abtop ships an update.

Current usage = last assistant's `input_tokens + cache_read_input_tokens`.
`cache_creation_input_tokens` is intentionally excluded — on compaction turns
the same tokens can be reported as both `cache_creation` and `cache_read`, and
summing all three double-counts.

## Polling Cadence

abtop is not push-driven. Pollers are staggered:

- session scan + transcript tail: **every 2s**
- process tree (`ps`): **every 2s**
- port scan (`lsof`/`netstat`) + git status + rate limits: **every 10s**

A session that starts, completes a tool call, and exits inside a 2s window can
be missed. A port that opens and closes inside a 10s window can be missed.

## OpenCode Feature Gaps

OpenCode support depends on `sqlite3` being on `PATH` and the local DB at
`~/.local/share/opencode/opencode.db`.

| Feature           | OpenCode |
|-------------------|----------|
| Session discovery | yes      |
| Token tracking    | yes      |
| Context window %  | no       |
| Status detection  | yes      |
| Current task name | no       |
| Rate limit panel  | no       |
| Subagents         | no       |
| Memory status     | no       |

When multiple OpenCode DB rows share one `cwd`, only live PIDs are matched;
older rows are not shown as duplicate live sessions.

## Terminal Size

- Minimum: **80x24**. Panels degrade gracefully: the context panel is hidden
  first, then mid-tier panels.
- Recommended: **120x40** or larger.

Below 80 columns the TUI switches to a narrow 4-tab layout
(`Workspace | Work | Usage | System`).

## tmux Session Jump

Pressing `Enter` to jump to a session's pane only works inside tmux.

- Outside tmux: Enter is a no-op.
- Inside tmux: if the agent PID is not found in any pane (for example the
  agent was started from a non-tmux terminal), abtop shows a transient
  "pane not found" status.

## Privacy Boundaries

abtop reads transcripts, prompts, tool inputs, subagent transcripts, and
memory files. These can contain secrets.

- `--once` and all `--workspace-summary`/`--roadmap`/`--handoff`/`--task-evidence`
  exports redact prompts, file contents, and absolute paths.
- The TUI shows tool name + first arg (a file path) only. File contents are
  never rendered.
- Session summary generation calls `claude --print` locally, which itself
  makes an API call. Set `ABTOP_DISPATCH_DISABLE_SUMMARIES=1` to skip
  summaries entirely.
- abtop never makes its own network calls.

## Mutating Controls

Three mutating actions are wired today:

- `x` — kill the selected session,
- `X` — kill all orphan ports,
- `d` — open the dispatch composer for the selected workspace task
  (`P6-UX-01`, opt-in per agent).

Kill actions require a confirmation keypress within
`KILL_CONFIRM_WINDOW_SECS` (2s); dispatch uses a separate
`DISPATCH_CONFIRM_WINDOW_SECS` (5s). All three verify the target before
mutating and write an append-only audit event for every outcome
(`requested`, `confirmed`, `skipped`, `blocked`, `sent`, `failed`,
`dry-run`).

Set `ABTOP_CONTROL_DRY_RUN=1` or `ABTOP_DISPATCH_DRY_RUN=1` to audit a
verified flow without actually killing or dispatching.

Disable mutating controls entirely in `~/.config/abtop/config.toml`:

```toml
allow_kill_sessions     = false
allow_kill_orphan_ports = false
allow_dispatch_claude   = false   # default
allow_dispatch_codex    = false   # default
allow_dispatch_opencode = false   # default
```

### Dispatch coverage

| Agent     | Status in current MVP                                              |
|-----------|--------------------------------------------------------------------|
| Claude    | wired via `claude --print` (stdin = brief + draft)                 |
| Codex     | wired via `codex exec` (best-effort; older builds may not support) |
| OpenCode  | **not wired** — no stable non-interactive surface yet; opting in   |
|           | with `allow_dispatch_opencode = true` emits a `Failed` audit event |
|           | until a documented command lands.                                  |

Dispatch responses are redacted (`collector::redact_secrets` +
`sanitize_terminal_text`), truncated at 256 KB, and written to
`{audit_dir}/dispatch/{rfc3339-ts}-{task-slug}-{agent}.md` so reviewers can
read them offline. The TUI only shows the byte count, outcome, and saved
path — never the response body.

## Deliberately Deferred

The following are explicit non-goals for the current milestone (per
`docs/AGENT_HANDOFF.md` and `docs/PRODUCTION_READINESS.md`):

- automatic task dispatch and reply,
- direct agent-to-agent private chat in abtop,
- cloud / team sync,
- RBAC,
- hosted dashboards,
- restart / archive / dispatch as mutating actions (need a policy + audit
  extension first — tracked as `P4-DSP-01` and `P6-UX-01` in
  `docs/EXECUTION_BOARD.md`).

These can be added later only after policy, audit, and redaction gates are
extended first.

## Reporting Issues

If a number on screen looks wrong, the most useful single artifact is:

```bash
abtop --doctor --json
```

It records collector health and which data sources were resolved, with no
prompt or file content.
