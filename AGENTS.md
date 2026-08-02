# abtop

AI agent monitor for your terminal. Like btop++, but for AI coding agents.

Supports Claude Code, Codex CLI, OpenCode, Grok, and Kimi Code sessions.

## Language Policy

English is mandatory for all project-facing work and communication.

- Write all source code, comments, tests, fixtures, documentation, examples, configuration text, scripts, and user-facing strings in English.
- Use English for every GitHub artifact: issue titles and bodies, issue comments, pull request titles and descriptions, review comments, commit messages, branch names, release notes, changelogs, discussions, labels, milestones, and workflow or CI messages.
- Do not use non-English text in repository content or GitHub communication unless it is an exact external identifier, a required protocol value, or a direct quote needed for context.
- When quoting or preserving non-English input, add an English explanation and keep the non-English text as short as possible.
- If a contributor opens an issue, comment, or review in another language, respond in English and continue the thread in English.

## Architecture

```
src/
├── main.rs                 # Thin binary entry that delegates to abtop::run
├── lib.rs                  # CLI dispatch, terminal setup/event loop, admin flags
├── app.rs                  # App state, tick logic, key handling, summary generation
├── config.rs               # Platform config loading and persisted UI preferences
├── demo.rs                 # Deterministic demo sessions and metrics
├── host_info.rs            # Host CPU/MEM and aggregate agent metrics
├── locale.rs               # Centralized English UI strings
├── snapshot.rs             # Stable JSON-friendly snapshot DTOs
├── theme.rs                # Built-in theme definitions and lookup
├── setup.rs                # Claude StatusLine installation only
├── codex_compat.rs         # 0.6-only direct native compatibility trampoline
├── codex_hooks/            # Native Codex plugin setup and content-free hook state
│   ├── mod.rs              # Internal plugin/state facade and silent ingest entry
│   ├── plugin.rs           # Isolated local marketplace/plugin bundle management
│   ├── migration.rs        # Exact legacy shell-wrapper marker cleanup
│   ├── ingest.rs           # Bounded hook parsing and native-process correlation
│   └── state.rs            # Private content-free lifecycle reducer/state store
├── jump/                   # cmux, tmux, and macOS iTerm2 terminal focus adapters
├── ui/                     # Responsive layout plus one module per panel/overlay
│   ├── mod.rs              # Desktop/narrow allocation and mouse hit-testing
│   ├── context.rs          # Token rate and per-session context gauges
│   ├── quota.rs            # Claude/Codex account quota windows
│   ├── tokens.rs           # Token totals and selected-session history
│   ├── projects.rs         # Per-project git state
│   ├── ports.rs            # Child listeners and orphan ports
│   ├── sessions.rs         # Session table, detail, timeline, and file audit
│   ├── mcp.rs              # Codex MCP server inventory and activity
│   └── {header,footer,help,config,view_menu}.rs
├── collector/
│   ├── mod.rs              # MultiCollector orchestration, orphan port detection
│   ├── claude.rs           # Claude Code: session discovery, transcript parsing
│   ├── codex.rs            # Hook lifecycle evidence + local rollout metrics
│   ├── opencode.rs         # OpenCode: session discovery via ps + SQLite DB parsing
│   ├── grok.rs             # Grok: active registry + session JSON/JSONL parsing
│   ├── kimi.rs             # Kimi Code: session index + wire JSONL parsing
│   ├── mcp.rs              # Codex mcp-server process/rollout discovery
│   ├── process.rs          # Child process tree (ps) + open ports (lsof) + git stats
│   └── rate_limit.rs       # Rate limit file reading (~/.claude/abtop-rate-limits.json)
└── model/
    ├── mod.rs              # Re-exports
    └── session.rs          # AgentSession, SessionStatus, RateLimitInfo,
                            # ChildProcess, OrphanPort, SubAgent
```

## Layout

The seven numbered panels are Context, Quota, Tokens, Projects, Ports, Sessions,
and MCP. Every panel can be toggled with `1`–`7`, persisted in platform config,
and hidden independently. Quota remains intentionally limited to Claude and Codex
unless another provider exposes a reliable local account-level source.

The supported minimum terminal is 60×18. Widths 60–99 use the tabbed Work / Usage /
System layout; widths of 100 or more use the desktop layout. Narrow sections divide
height evenly unless the active section is maximized. Desktop allocation reserves the
one-row header/footer, gives Sessions a five-row minimum and first claim toward its
ideal height, then assigns the mid-tier row; Context appears only when the requested
Sessions height is satisfied and at least five surplus rows remain. At 120×40 and above
the full desktop layout is normally visible.

The Sessions panel contains the list and selected-session detail, including evidence,
children, subagents, chat, timeline, and file audit. The MCP panel shows detected Codex
`mcp-server` processes and recent rollout activity. MCP-owned rollouts are suppressed
from Sessions by default; `M` changes suppression for the current run only.

## Data Sources

Collectors are read-only over local files plus local process and port metadata
(`ps`/`lsof` on Unix, native equivalents on Windows). No provider API calls or
authentication are used. The explicit setup commands write only their documented
Claude or Codex integration, and the installed Codex helper writes only its private,
bounded lifecycle state.

### 1. Claude Code session discovery: process + config-root mapping

Discovery strategy:
1. Find running `claude` processes via `ps`
2. Map PID → open files/directories via `lsof`
3. Infer Claude config roots from open paths that contain `sessions/` and `projects/`
4. Read `{config-root}/sessions/{PID}.json`, falling back to scanning session files for the matching embedded PID
5. Parse `{config-root}/projects/{encoded-path}/{sessionId}.jsonl`

Fallback config roots are still scanned: `~/.claude`, direct home profile roots matching `~/.claude-*` when they contain both `sessions/` and `projects/`, `claude_config_dirs` from `~/.config/abtop/config.toml`, abtop's own `CLAUDE_CONFIG_DIR`, and on Linux any `CLAUDE_CONFIG_DIR` read from `/proc/{pid}/environ`.

Session file format:
```json
{ "pid": 7336, "sessionId": "2f029acc-...", "cwd": "/Users/graykode/abtop", "startedAt": 1774715116826, "kind": "interactive", "entrypoint": "cli" }
```
- ~170 bytes. Created on start, deleted on exit.
- Verify PID alive with shared `ps` data containing a `claude` binary.
- Skip sessions whose PID descends from abtop's own `claude --print` summary children without hiding user-spawned non-interactive sessions.

### 2. Claude Code transcript: `{config-root}/projects/{encoded-path}/{sessionId}.jsonl`
Path encoding: `/Users/foo/bar` → `-Users-foo-bar`

Key line types:

**`assistant`** (tokens, model, tools):
```json
{
  "type": "assistant",
  "timestamp": "2026-03-28T15:25:55.123Z",
  "message": {
    "model": "claude-opus-4-6",
    "stop_reason": "end_turn",
    "usage": {
      "input_tokens": 2,
      "output_tokens": 5,
      "cache_read_input_tokens": 11313,
      "cache_creation_input_tokens": 4350
    },
    "content": [
      { "type": "text", "text": "..." },
      { "type": "tool_use", "name": "Edit", "input": { "file_path": "src/main.rs", ... } }
    ]
  }
}
```

**`user`** (prompts, version):
```json
{ "type": "user", "timestamp": "...", "version": "2.1.86", "gitBranch": "main", "message": { "role": "user", "content": "..." } }
```

**`last-prompt`** (session tail marker):
```json
{ "type": "last-prompt", "lastPrompt": "...", "sessionId": "..." }
```

- **Size: 1KB–18MB**. Append-only, new line per message.
- **Reading strategy**: On first discovery, scan full file to build cumulative token totals. Then watch file size — on growth, read only new bytes appended since last read (track file offset). This gives both lifetime totals and real-time updates without re-reading.
- **Partial line handling**: new bytes may end mid-JSON-line. Buffer incomplete lines until next read.
- **File rotation**: if file shrinks (session restart), reset offset to 0 and re-scan.

### 3. Codex CLI sessions: native plugin hooks + local rollouts

On macOS and Linux, `abtop --setup-codex` installs an isolated local Codex plugin in the
active `${CODEX_HOME:-~/.codex}`. Windows keeps read-only process/rollout metrics but
reports Codex lifecycle as unmanaged `Unknown`; setup fails before mutation there.
The integration must never alias, wrap, replace, or proxy the native
`codex` command. Plain `codex`, `codex resume`, `codex fork`, `codex --yolo`, and every
other native argument retain their normal behavior.

Plugin layout:

```text
$CODEX_HOME/abtop/marketplace/
├── .agents/plugins/marketplace.json
└── plugins/abtop/
    ├── .codex-plugin/plugin.json
    ├── hooks/hooks.json
    └── scripts/abtop-codex-hook.{sh,cmd}
```

Setup and migration rules:

1. Resolve and retain the exact lexical `codex` entry selected from `PATH` whose
   invocation proves `codex-cli 0.146.0`; do not canonicalize away argv-sensitive shims.
2. Safely remove only legacy shell blocks delimited by
   `# >>> abtop managed codex >>>` and `# <<< abtop managed codex <<<`. A missing block
   is success. Malformed, unmatched, or duplicate markers fail closed without changing
   the file. Preserve unrelated profile content, aliases, and functions. Apply the same
   exact-marker policy to zsh, bash, and fish. Windows setup exits before migration;
   successful Windows uninstall may provide exact manual PowerShell cleanup guidance.
   On macOS/Linux, lock and revalidate every target, use atomic replacements, and perform
   a lost-update-safe rollback if migration cannot complete. Serialize Unix scans and
   edits with the stable, content-free mode-`0600`
   `~/.abtop-codex-migration.lock`. Status inspection may create this file; retain its
   inode so concurrent processes cannot lock different replacements. The non-Unix lock
   is a no-op. When zsh is the active shell, setup, uninstall, and status inspection
   may run strictly framed, output-bounded login and non-login probes so an unexported
   custom `ZDOTDIR` is included. Those probes evaluate normal zsh startup files but
   persist none of their content.
3. Serialize setup and uninstall with the stable
   `$CODEX_HOME/.abtop-codex-plugin.lock`. On Unix require a same-owner regular file with
   mode `0600`, revalidate its inode while held, and retain it after uninstall so
   concurrent administrative processes cannot lock different replacements. Treat
   `$CODEX_HOME/abtop/.setup.lock` only as legacy source-bundle debris that may be removed
   during verified owned-source cleanup; it is never the current lock.
4. Require exactly `codex-cli` 0.146.0, exact `hooks stable true` and
   `plugins stable true` feature rows, and exactly the supported 11 uppercase
   `ManagedHooksRequirements.properties` in generated
   `v2/ConfigRequirementsReadResponse.json`.
   The short-lived `codex app-server generate-json-schema` preflight is schema inspection
   only; it is never a relay, supervisor, monitoring transport, or persistent daemon.
   Give every native administrative command null stdin, a 15-second overall timeout, and
   independent 1 MiB stdout/stderr caps. On Unix use a separate process group,
   nonblocking pipe drains, group termination on timeout or a pipe retained for 100 ms
   after leader exit, and bounded reap. On Windows use a kill-on-close Job Object when
   available plus direct-child fallback; portable reader channels must never be joined
   indefinitely. Bracket every mutating invocation with the exact selected executable
   identity digest.
   Generate the marketplace/plugin bundle with private permissions. Its version and
   hook command incorporate the hook schema revision and helper identity digest.
5. Use native `codex plugin marketplace add <marketplace-root> --json` and
   `codex plugin add abtop@abtop-local --json`, then verify that the plugin is installed
   and enabled. Never write trusted hashes or bypass hook trust.
6. Tell the user to restart plain native Codex and approve only the 11 hooks attributed
   to `abtop@abtop-local`. A changed executable path or byte content changes the helper
   identity. Rerun setup after every binary update or replacement regardless; require a
   fresh review whenever Codex presents the changed identity.

`abtop --uninstall-codex` applies the same exact-marker cleanup and preserves the
content-free plugin-data directory. It always invokes removal of the reserved
`abtop@abtop-local` plugin ID first, even if the marketplace is missing, malformed, or
conflicting, and verifies that registration is absent before marketplace/source cleanup.
It may remove marketplace registration/source only after proving the exact owned local
source, then verifies marketplace and plugin absence again before deleting source;
otherwise preserve them and return manual-recovery guidance. It retains the stable root
setup lock and the plugin-data tree. Unlike
setup/status compatibility, uninstall accepts any exact stable `X.Y.Z` Codex semver as a
recovery path after a native Codex downgrade or upgrade.
`abtop --codex-integration-status` audits compatibility, source and cached bundles,
attestation, declaration, installation, base trust/enablement, and legacy cleanup. The
base audit is diagnostic installation evidence, not proof of a live thread's effective
in-memory hook engine: profiles, SessionFlags, project/config-lock layers, managed
policy, and live reload can differ. It
exits 0 only when healthy and 1 when not ready or inspection fails; malformed singleton
administration invocations exit 2. A healthy base audit never overrides a per-process
profile, trust bypass, or command-line/config hook override; that process remains
`Unknown`. `abtop --setup` remains Claude-only. Existing global Codex hooks, `notify`,
OpenTelemetry, plugins, and unrelated configuration must remain unchanged.

The plugin installs these 11 matcherless hooks, each synchronous, silent, and limited
to one second:

- `PreToolUse`
- `PermissionRequest`
- `PostToolUse`
- `PreCompact`
- `PostCompact`
- `SessionStart`
- `SessionEnd`
- `UserPromptSubmit`
- `SubagentStart`
- `SubagentStop`
- `Stop`

Do not install `PostToolUseFailure`; Codex 0.146.0 does not advertise it as a supported
plugin event. The launcher creates the no-clobber fault marker and invokes the hidden
abtop ingest command; the ingest helper drains JSON input. Both absorb all errors, and
the launcher exits 0 without stdout or stderr so a failed monitor cannot deny a Codex
action after the bounded hook returns.

Release 0.6 retains hidden `abtop codex -- ...` only as a no-relay compatibility
trampoline for already-loaded legacy wrapper functions. It requires
`ABTOP_MANAGED_CODEX_BINARY`, invokes that exact captured binary with exact arguments
and standard streams, and preserves its exit behavior. Unix process replacement also
preserves native signal behavior. It has no fallback, argument allowlist, manifest, or
monitoring role. Remove it in 0.7 and never present it as the normal launch path.

Hook ingest and state rules:

1. Stream at most 4 MiB through serde without materializing the raw payload. Accept at
   most 256 root fields, 512-byte lifecycle IDs, a 16 KiB cwd, and other bounded
   allowlisted scalars. Skip all other values with `IgnoredAny` and attempt to drain to
   EOF after malformed JSON. Prompt text, tool input/output, last-assistant text, raw
   argv/commands, environment/authentication, transcript paths, and arbitrary provider
   text must never enter state.
2. Store state only below `$CODEX_HOME/plugins/data/abtop-abtop-local`, using private
   mode-0700 directories and mode-0600 files on Unix, same-owner validation, no-follow
   opens, symlink-escape rejection, locks, and same-directory atomic replacements.
3. Treat `session_id` as the shared root identity on root and descendant hooks. Treat
   `agent_id` only as a child-agent identity and fold child lifecycle into the shared
   root state; never create a separate session from it.
4. Bound all state and retain at most 128 content-free samples. Persist only
   schema/helper/install identities, session/turn/tool/subagent IDs, canonical event
   kind and tool class, cwd when correlation requires it, timestamps, exact PID/start
   incarnations, and faults/open sets.
5. Before starting abtop, the POSIX launcher must first use `mktemp` to exclusively
   create an empty private mode-`0600`
   `launch-<shell-pid>-pending.<16-alphanumeric-nonce>` marker in the embedded fault
   directory. Only if unique allocation fails may it use the 16 fixed no-clobber
   `launch-<slot>-abtopv1.pending` names; persistent `overflow.json` is the exhaustion
   fallback. The helper anchors the directory, adopts only that exact marker/inode, and
   enriches it to bounded content-free JSON with a fresh random 128-bit per-adoption
   commit ID before attestation, ancestor resolution, parsing, or folding. A
   missing/rejected token independently attempts a generic `hook-<id>.json` marker.
   Commit the marker basename plus commit ID with the folded state before unlinking that
   same marker. Preserve valid commit proofs across clean startup/resume/clear boundaries;
   reject legacy basename-only proofs so fixed-name reuse cannot impersonate another
   invocation.
   An empty/malformed marker is global `Unknown`; a timeout, failed launch, crash,
   malformed event, changed config/helper, or unsafe association must never preserve a
   stale positive row. Bound fault/state/temp files. Before deleting generation state,
   persist the writer's first GC-side confirmation that the exact process incarnation is
   gone. That observation is only a deletion grace anchor and never authorizes `Done`.
   Deletion requires a later writer pass strictly more than 30 seconds afterward and a
   fresh exact-incarnation gone check. Capacity pressure starts this sequence only after a
   terminal generation is at least 30 seconds old or a crashed nonterminal generation is
   strictly older than 24 hours; normal cleanup runs on a later `SessionEnd` and requires
   the strict 24-hour gate for either. After payload drain, every later ingest may reclaim
   strictly older-than-24-hour state/fault temporary files, malformed or abandoned
   fixed-slot markers, and ordinary fault markers only from a complete validated state
   snapshot that remains unchanged across out-of-lock process-death probes, with every
   affected process incarnation confirmed gone. Collector reads never clean state,
   `overflow.json` is permanent and monotonic, and a clean `SessionStart` never deletes
   failure evidence.
6. Because hook input has no PID, identify the nearest eligible native Codex ancestor
   and double-read its process incarnation. Exclude app-server/daemon, `mcp-server`,
   Desktop, and remote-control hosts. An npm wrapper can support discovery but the state
   must bind to its native Codex child. Shared-daemon hooks and PID ambiguity are
   `Unknown` and inactionable. Never start or attach to a daemon.
7. Keep action PID/session ownership separate from lifecycle evidence. Unknown ownership
   disables kill and terminal jump even when a lifecycle edge is otherwise useful.

Codex 0.146.0 exposes no thread/PID/generation-bound attestation of the effective hook
engine after profile, project/config-lock, managed/cloud, command-line, per-thread, and
live-reload layers are applied. Base installation integrity, trust/enablement, individual
hook events, and rollout correlation are not live-coverage proof. Production must keep
`effective_hook_engine_attested = false`, so every live Codex `Thinking`, `Executing`,
and `Idle` candidate projects as non-actionable `Unknown / Unavailable`. Retain the raw
bounded lifecycle candidate only for audit and independent exit correlation.

Exit proof requires exact installation, process, session, and event correlation. Require
the matched process-owned root rollout to report exact `cli_version = "0.146.0"`, and
require every discovered descendant rollout to report the same version. Missing,
different, child-only, or descendant-mismatched version metadata is insufficient. Then
apply this matrix:

- A previously observed exact live PID/start ↔ supported-version rollout-tree binding,
  followed by that same process incarnation changing from live to gone, creates a
  bounded, content-free, non-actionable in-memory tombstone
  → `Done / Heuristic` through 30 seconds from the collector's transition observation.
  Source-state disappearance or an unavailable scan may preserve that tombstone, but a
  collector whose first observation is already gone, numeric-PID reuse without exact
  incarnation continuity, or an unavailable scan cannot create one.
- A root open tool, a child open tool, `PermissionRequest`, or a
  `request_user_input` candidate → `Unknown / Unavailable`. `PreToolUse` runs before a
  separately configurable permission edge, so an open hook/rollout call cannot prove
  that execution began.
- Any otherwise exact lifecycle shape suggesting root model work, child model work, or
  turn completion → `Unknown / Unavailable`, normally with
  `HookIntegrationUnverified`, because effective live hook coverage is unattested.
- Every `SessionStart` source is generation evidence only and is never sufficient for
  `Idle`.
- Rollout `stream_error` or `error`, a `task_complete` carrying a terminal error, any
  unparseable open descriptor, or a nonterminal non-selected root tree invalidates
  rollout lifecycle and cannot seed new exit proof. A direct-child
  active/terminal/provisional mismatch or a child `PreToolUse` without its exact close is
  also live `Unknown`, without treating incomplete child lifecycle as work or rest.
- `Stop` alone, hosted/uncovered/background tools, missing/out-of-order events, failure
  without an exact close, incomplete/aborted/stale/duplicate/mismatched child lifecycle,
  unsupported active/non-direct child lifecycle, config drift, and PID/session ambiguity
  → `Unknown / Unavailable`. Exact-terminal nested descendants remain internal
  correlation evidence only and never authorize public live-status promotion.

Codex queues startup, resume, and clear `SessionStart` hooks into the next turn before
`UserPromptSubmit`; those sources reset the generation. Compact `SessionStart` follows
`PostCompact` inside the current turn and preserves active work. No source is `Idle`
proof. A newly opened empty composer can have no evidence and remains `Unknown`.

`Stop` and `SubagentStop` are provisional: another hook can block them, and the same
actor can continue within the turn. Later matching activity reopens the actor. A child
stop closes internally only after exact matching child `task_complete`; later active
child model work reopens the internal candidate. The public live row remains `Unknown`
in both cases.

Codex 0.146.0 does not expose sufficient prompt-display and resolution lifecycle to label
approvals or questions safely. Therefore abtop deliberately reports those states as
`Unknown`, never guesses `Waiting`, and never leaves a stale `Executing` label. Hook data
does not create live Codex `Error` or `RateLimited`; account rate limits remain quota
metadata. Never infer live status from elapsed time, CPU use, transcript/rollout mtime,
token activity, or uncorrelated child-process activity.

Codex-specific content-free `StatusReason` values include
`HookIntegrationUnverified`, `HookConfigChanged`, `HookEventGap`,
`HookStateMalformed`, `HookInteractionResolutionUnavailable`, `HookToolOpen`,
`HookSubagentActive`, `HookTurnOpen`, and `HookTurnComplete`; generic failure/transition
paths can also use `OwnershipUnconfirmed` and `ProcessExited`. Hook evidence always uses
`connection_generation = 0`.

Codex CLI processes without a valid current plugin state, Codex Desktop, and other
hosted sessions remain discoverable through shared process/open-file evidence and local
rollouts, but their live status is `Unknown`.

Rollout files live at
`${CODEX_HOME:-~/.codex}/sessions/YYYY/MM/DD/rollout-*.jsonl`. Parsed rollouts are
cached only after descriptor-bracketed validation of the canonical path, file identity,
length, modification time, and platform change/creation time. The bounded cache
invalidates on append, replacement, same-length rewrite, or pathname/descriptor drift so
a process holding many dormant subagent descriptors does not force a full rescan every
tick. Build the `parent_thread_id` graph and aggregate descendants into the selected root
row instead of showing subagents as duplicate top-level sessions.

Rate limits extracted from `token_count` events:
```json
{
  "rate_limits": {
    "limit_id": "codex",
    "primary": { "used_percent": 9.0, "window_minutes": 300, "resets_at": 1774686045 },
    "secondary": { "used_percent": 14.0, "window_minutes": 10080, "resets_at": 1775186466 },
    "plan_type": "plus"
  }
}
```

- Rollout tool/request records remain useful for metrics and display metadata, but never
  determine Codex status without a matching hook lifecycle edge.
- A previously observed exact live PID/start ↔ supported-version rollout-tree binding,
  followed by that same incarnation changing from live to gone, produces heuristic `Done`
  through 30 seconds from the collector's transition observation. A bounded,
  content-free, non-actionable in-memory tombstone preserves the row if source state
  disappears. An unavailable scan may retain existing proof but cannot create it; a
  collector whose first observation is already gone or numeric-PID reuse without exact
  incarnation continuity never produces `Done`. Independently, writer-side GC persists
  its first exact-incarnation gone confirmation and may delete state only on a later pass
  strictly more than 30 seconds afterward after a fresh gone check. Capacity pressure
  begins that sequence only once terminal state is at least 30 seconds old or crashed
  nonterminal state is strictly older than 24 hours; normal cleanup runs on a later
  `SessionEnd` and requires the strict 24-hour gate for either. Collector reads never
  perform cleanup.
- Historical rollout files must never create PID-zero `Done` rows.

### 4. OpenCode sessions: `~/.local/share/opencode/opencode.db`
- Discover running `opencode` processes via shared `ps` data.
- Read recent sessions from OpenCode's SQLite DB through `sqlite3 -readonly -json`.
- Read only the latest message/tool-part lifecycle on fast ticks; never select raw prompt, tool input, or tool output data for status detection.
- Match live PIDs to DB sessions by process cwd. OpenCode does not expose a PID/session mapping, so when multiple DB rows share one cwd, only live PIDs should be assigned and older rows should not be shown as live duplicates.
- OpenCode contributes session/token/project/port data, but not quota data. Quota remains Claude + Codex only.

### 5. Grok sessions: `${GROK_HOME:-~/.grok}/sessions/`

Discovery strategy:
1. Find running `grok`, `agent`, `xai-grok-pager`, and managed Grok platform binaries via shared `ps` data. Exclude leader PIDs using Grok's `leader*.lock` files; command text alone is insufficient because some platforms flatten argv boundaries.
2. Read `${GROK_HOME:-~/.grok}/active_sessions.json`, whose entries contain `session_id`, `pid`, `cwd`, and `opened_at`.
3. Verify the PID still belongs to Grok and that its process start predates `opened_at` before trusting the registry entry. This guards against stale entries and PID reuse.
4. Read the session's `summary.json`, `signals.json`, append-only `updates.jsonl`, optional append-only `events.jsonl`, and `plan_mode.json`.
5. Headless mode is visible when Grok registers it in the same active-session registry (for example with `GROK_TRACK_HEADLESS`). Unregistered processes are not guessed from cwd alone; leader/ACP hosts that do not represent a user-owned session are excluded.

Important behavior:
- `summary.json` supplies session identity, cwd, timestamps, current model, and title metadata.
- `signals.json` is preferred for current context tokens/window and turn count.
- `updates.jsonl` supplies durable token usage, tool lifecycle, status, and subagent events. Tail it incrementally and tolerate both wrapped ACP records and xAI-specific events.
- `events.jsonl` pairs permission requests and resolutions. An unmatched request from the current registry-open interval sets `awaiting_input`; malformed or unavailable event logs must not hide an otherwise valid session.
- An unresolved canonical `ask_user_question` tool or `plan_mode.json` with `awaiting_plan_approval = true` also marks the session as waiting for user input.
- A Grok TUI process can own multiple sessions. Show every registered session, but attribute process memory, children, and ports only to its most recently active row so aggregate values are not double-counted.
- Killing any row backed by a shared PID kills the process and therefore all sessions it owns. The confirmation message must state how many sessions will be affected.
- Grok contributes no quota data. The quota panel remains Claude + Codex only.

### 6. Kimi Code sessions: `${KIMI_CODE_HOME:-~/.kimi-code}/sessions/`

This collector targets the current [Kimi Code](https://github.com/MoonshotAI/kimi-code) CLI. The retired `MoonshotAI/kimi-cli` format under `~/.kimi` is out of scope.

Discovery strategy:
1. Find running `kimi-code`, `kimi`, and the Kimi Code Node wrapper via shared `ps` data; exclude plugin-runner, ACP, and web host modes when their arguments are visible.
2. Fold append-only `${KIMI_CODE_HOME:-~/.kimi-code}/session_index.jsonl`, including deletion tombstones.
3. Validate that every indexed session path stays below the configured `sessions/` root, contains no symlink escape, and has a basename matching the session ID.
4. Read per-session `state.json` plus `agents/main/wire.jsonl`. Accept current v1/v2 state timestamp and cwd field variants.
5. Prefer an explicit session ID from the process command. Otherwise correlate within the process's own root by cwd and recent activity, keeping assignments stable between polls and following a newer post-start activity edge when one PID switches sessions in place.

Kimi Code has no authoritative PID/session registry and rewrites its process title to `kimi-code`, which can hide both session flags and host subcommands. A unique root+cwd mapping becomes actionable only after an explicit match or session activity at/after the process start; idle old resumes remain `Unknown`. If multiple live Kimi processes share one root+cwd, pair rows deterministically but keep ownership `Unknown`. Unknown rows cannot be killed or terminal-jumped from abtop. Use separate worktrees when authoritative live ownership is important.

Wire behavior:
- Sum only `usage.record` events for lifetime tokens; `step.end.usage` duplicates those totals.
- Derive current tools and status from interaction, turn, step, tool-call, tool-result, and validated task-lifecycle records. Every unresolved `AskUserQuestion`, whether foreground or background, and every validated running `question` task, including detached tasks, sets `awaiting_input` and reports `Waiting`. Exact resolution, cancellation, tool completion, or a terminal task snapshot clears the corresponding wait.
- Resolve context limits from the active model alias in Kimi Code's TOML configuration. If no reliable limit is available, keep `context_window = 0` and display `—` instead of guessing.
- Build subagents from state topology, task lifecycle records, and child agent wires.
- Kimi contributes no quota data. The quota panel remains Claude + Codex only.

Custom Grok and Kimi roots are discovered from `GROK_HOME` / `KIMI_CODE_HOME` inherited by abtop. abtop also makes a best-effort platform-specific read of each candidate process environment; launch abtop with the same environment as the agent when the operating system does not expose it.

### 7. Subagents: provider-local sources

Claude Code stores subagents under `~/.claude/projects/{path}/{sessionId}/subagents/`:
- `agent-{hash}.jsonl` — same JSONL format as main transcript
- `agent-{hash}.meta.json` — `{ "agentType": "general-purpose", "description": "..." }`

Grok emits subagent lifecycle updates and may persist child sessions below a parent's `subagents/` directory. Kimi Code stores each child agent wire under `agents/{agentId}/wire.jsonl` and records its topology in `state.json`.

### 8. Process tree: `ps` + `lsof`
```bash
ps -eo pid,ppid,rss,%cpu,command    # All processes
lsof -i -P -n -sTCP:LISTEN         # Open ports
```
- Build parent→children map from ppid
- Map listening PID → parent agent PID → session

### 9. Git status per project
```bash
git -C {cwd} status --porcelain     # added/modified file counts
```

### 10. Memory status
- Path: `~/.claude/projects/{encoded-path}/memory/`
- Count files in directory + lines in `MEMORY.md`

### 11. Rate limit (Claude Code)

NOT in transcript JSONL. Collected via StatusLine mechanism.

`abtop --setup` automates this: creates a script at `~/.claude/abtop-statusline.sh` that writes rate limit JSON to `~/.claude/abtop-rate-limits.json`, and registers it in `~/.claude/settings.json`.
This command is Claude-only. It does not inspect, preflight, or modify Codex. Use the
separate explicit `abtop --setup-codex` command for Codex plugin integration.

File format read by abtop:
```json
{
  "source": "claude",
  "five_hour": { "used_percentage": 35.0, "resets_at": 1774715000 },
  "seven_day": { "used_percentage": 12.0, "resets_at": 1775320000 },
  "updated_at": 1774714400
}
```
- Rejects stale data (> 10 minutes old).
- `rate_limits` only present for Pro/Max subscribers.
- Account-level metric, shared across all sessions.
- Show "—" when not configured or data unavailable.

### 12. Other files
- `~/.claude/stats-cache.json` — daily aggregates. Only updated on `/stats`, NOT real-time.
- `~/.claude/history.jsonl` — prompt history with sessionId.

## Session Status Detection

```
◉ Think        = model turn is open and no tool is currently running
● Exec         = trustworthy provider-specific lifecycle evidence proves active work
◌ Wait         = an exact unresolved interaction requires user action
○ Idle         = live session has no current model, tool, task, or interaction work
? Unknown      = trustworthy current lifecycle proof is insufficient
⏳ Rate         = provider reports an active account-level rate limit
✗ Error        = provider reports a live fatal session/turn failure
✓ Done         = verified process has exited
```

For Codex, an open root or child tool is deliberately `Unknown`, not `Exec`, because the
0.146.0 hook contract cannot prove whether permission is still pending. Exact active
subagent model work remains an internal correlation candidate only; effective live hook
coverage is unattested, so the public row remains `Unknown`.

After ownership/liveness and protocol validation, exact status precedence is `Waiting` >
`RateLimited` > `Error` > `Executing` > `Thinking` > `Idle`. A real user-action wait wins
when background work continues at the same time. `Unknown` covers any case without
sufficient trustworthy lifecycle proof, including ownership ambiguity, stale evidence,
disconnects, malformed protocol state, and failed validation. Never infer `Waiting` from
elapsed time, transcript mtime, or low CPU usage.

Every row carries `StatusEvidence`: authority (`Provider`, `Heuristic`, or `Unavailable`),
a machine-readable reason, observation/status-since timestamps, connection generation,
consecutive matching count, and a bounded content-free sample history. Codex hook
evidence always uses connection generation zero and is at most `Heuristic`; unsafe or
insufficient Codex evidence is `Unavailable`. The selected-session detail and `--once` expose current
authority, reason, observation time/freshness, connection generation, consecutive
matching count, and the statuses of the latest five samples. JSON snapshots include the
same current fields and the latest five complete content-free samples for audit.

**Done detection**: Codex requires this collector instance to have previously observed an
exact live PID/start ↔ supported-version rollout-tree binding, followed by that same
process incarnation changing from live to gone. It retains a bounded, content-free,
non-actionable in-memory tombstone through 30 seconds from that observation, including if
source state later disappears. An unavailable scan may preserve an existing tombstone but
cannot create one; a collector whose first observation is already gone or numeric-PID
reuse without exact incarnation continuity never fabricates `Done`. Historical Codex
rollouts never become PID-zero Done rows. Other providers use their documented process
or registry evidence and may disappear immediately after verified exit.

**PID reuse risk**: verify the exact process incarnation and provider-native live argv
before jumping or killing. For Grok registry entries, also compare process start time
with `opened_at`. Never trust PID alone.

Current task (2nd line under each session):
- Thinking → "thinking"
- Executing → provider-safe work preview. Codex 0.146.0 live rows never reach this
  status because effective hook coverage is unattested.
- Waiting → "waiting for user input"
- Idle → "idle"
- Error → canonical provider failure label; never raw provider error content
- Unknown → insufficient-evidence warning; killing is disabled
- Done → "finished"

**Known limitations** (all best-effort):
- Explicit provider interaction signals take precedence, but not every provider persists every kind of wait.
- Codex hook evidence is never provider-authoritative. A missing, stale, unsafe,
  malformed, ambiguous, or uncorrelated hook generation becomes `Unknown`.
- Plain Codex may use a local or remote shared app-server daemon. Hooks then run below
  that shared host rather than the client TUI, so they cannot be bound to its PID and
  remain `Unknown` and inactionable.
- Codex 0.146.0 approvals and questions are deliberately `Unknown`, not `Waiting`, because
  exact prompt-display/resolution lifecycle is unavailable.
- Codex 0.146.0 exposes no thread/PID/generation-bound attestation of the effective hook
  engine. Base hook trust and enablement are installation diagnostics only. Root and
  child open tools remain `Unknown` because a selectively missing `PermissionRequest`
  is indistinguishable from execution. All live `Thinking`, `Executing`, and `Idle`
  candidates remain non-actionable `Unknown / Unavailable`; only an independently exact
  supported process/session `Live → Gone` transition can promote to non-actionable
  heuristic `Done` for 30 seconds.
- Never infer Codex status from rollout ordering/content alone, token activity, mtime,
  CPU use, child processes, or open tool records. Those sources provide metrics and
  correlation metadata only.
- OpenCode does not persist live permission prompts in SQLite, so abtop cannot label those waits authoritatively.
- Kimi v1 does not persist ordinary tool-approval prompts. Those remain `Executing`; abtop does not guess from elapsed time or process inactivity.
- A long-running Codex root tool without a provider execution-start signal remains
  `Unknown`; never turn an open call into `Executing` merely because it persists.
- Provider status is best-effort unless its evidence explicitly says `Provider`; Codex
  live hook evidence is `Unavailable`, while its exact transition-bound `Done` is capped
  at `Heuristic`.
- Kimi Code does not provide an authoritative PID/session map. Same-cwd ambiguity is reported as `Unknown` ownership and disables killing.
- One Grok process may own several sessions. Process resources are attributed once, and killing that PID stops all of its sessions.
- Grok and Kimi context percentages are unavailable when their local signals/configuration do not expose a reliable window size.

## Session Summary Generation

Each session gets a one-line summary title generated via `claude --print`:
- Spawned as background process with 10s timeout
- Sends at most 200 characters each from the first user and first assistant text
- Rejects generic/empty output; falls back to a sanitized 80-character excerpt
- Caches only generated titles or sanitized fallbacks in the platform cache directory
  (`${XDG_CACHE_HOME:-~/.cache}/abtop`, `~/Library/Caches/abtop`, or
  `%LOCALAPPDATA%\abtop`)
- Max 3 concurrent summary jobs, max 2 retries per session
- `App::tick_no_summaries()` and `abtop --json` never spawn summary jobs

## Context Window Calculation

Context sources are provider-specific:

- Claude uses 200,000 by default and 1,000,000 when the transcript/configured model
  contains `[1m]` or observed context exceeds 200,000. Current usage is normally
  `input_tokens + cache_read_input_tokens`; for a fresh session with zero cache read and
  nonzero cache creation, use `input_tokens + cache_creation_input_tokens`. This avoids
  double counting compaction turns while representing the initial cache correctly.
- Codex uses the rollout's explicit `model_context_window` and last token-usage record.
- OpenCode estimates 200,000, or 1,000,000 for a `[1m]` model.
- Grok uses local signal fields and Kimi uses its resolved local model configuration.
  Unknown provider limits remain unavailable rather than guessed.

Context percentage is current usage divided by the validated window. The Context panel
shows `!` at 75% and `⚠` at 90%; bar color follows the theme gradient.

## Orphan Port Detection

Tracks child processes that have open ports. When a parent session dies but the child process remains alive and listening:
- Added to `orphan_ports` list automatically
- Displayed in ports panel under "ORPHAN PORTS" section
- Can be killed via `X` (Shift+X) only after a fresh port scan, exact PID/command
  verification, and the platform termination path

## Key Bindings

| Key | Action |
| --- | ------ |
| `↑`/`↓`, `k`/`j` | Select the previous or next visible session |
| `Enter` | Jump to an actionable terminal after fresh process validation |
| `/` | Enter filter mode; `Backspace` edits, `Enter` keeps, `Esc` clears |
| `x` twice within two seconds | Kill the same freshly revalidated session/process incarnation |
| `X` | Freshly rescan and kill validated orphan-port owners |
| `r`, `q` | Force refresh or quit |
| `t`, `T`, `l`/`L`, `f`/`F` | Cycle theme, toggle tree, timeline, or file audit |
| `1`–`7`, `M` | Toggle/persist panels or toggle runtime MCP-session suppression |
| `c`, `v`, `?` | Open config, view menu, or help |

In the narrow layout, arrows/Tab cycle visible tabs, `w`/`u`/`s` select Work /
Usage / System, `+` or `=` maximizes the active section, and `-` restores the split.
Demo mode uses the default panel set and default theme unless `--theme` is explicit; it
ignores persisted hidden-agent, custom Claude-root, visibility, and theme settings. It
disables keyboard `r`, `x`, `X`, and `Enter`; mouse orphan cleanup remains live when
`--demo --mouse` is explicitly used.

## Tech Stack

- **Rust** (2021 edition)
- **ratatui** + **crossterm** for TUI
- **serde** + **serde_json** for JSON/JSONL parsing
- **toml** for native Codex configuration and plugin inspection
- **chrono** for timestamp formatting
- **dirs** for home directory resolution
- **tempfile**, **sha2**, and **getrandom** for bounded private integration state
- Platform support through **libc**, **proc_pidinfo**, **sysinfo**, and **windows-sys**
- **Polling intervals** (staggered to avoid freezes):
  - Session scan + transcript tail: every 2s
  - Process tree (ps): every 2s
  - Port scan + git status: every 10s, with immediate port invalidation when the PID set changes
  - Rate-limit files: first tick and then approximately every 10s

## Commit Convention

```
<type>: <description>
```
Types: `feat`, `fix`, `refactor`, `docs`, `chore`

## Commands

```bash
cargo build                    # Build
cargo run                      # Run TUI
cargo run -- --once            # Print snapshot and exit
cargo run -- --json            # Print a summary-free JSON snapshot
cargo run -- --demo            # Run deterministic, non-keyboard-destructive demo data
cargo run -- --mouse           # Opt in to mouse capture and click/wheel targets
cargo run -- --theme btop      # Launch-only theme override
cargo run -- --setup           # Install the Claude quota hook only
cargo run -- --setup-codex     # Install/repair the local Codex plugin (macOS/Linux)
cargo run -- --uninstall-codex # Remove the local abtop Codex plugin integration
cargo run -- --codex-integration-status # Audit the Codex plugin integration
codex                          # Native Codex command; never wrapped by abtop
codex resume                   # Native resume
codex fork                     # Native fork
codex --yolo                   # Native flags remain untouched
cargo run -- --exit-on-jump    # Quit after Enter-jumping to a session terminal (for popup overlays)
cargo fmt --all -- --check     # Formatting verification
cargo test                     # Tests
cargo clippy --all-targets -- -D warnings # Strict lint
cargo build --release          # Release build
cargo publish --dry-run --allow-dirty # Package validation before the PR is committed
```

Release 0.6's hidden `cargo run -- codex -- ...` path exists only so an already-loaded
legacy abtop wrapper can delegate to its captured native Codex binary. Do not use or
document it as a monitoring or launch workflow; remove it in 0.7.

## Release Process

1. Pick the target semver version and update both `Cargo.toml` and `Cargo.lock`.
2. Verify the package locally:
   ```bash
   cargo test
   cargo clippy -- -D warnings
   cargo build --release
   cargo publish --dry-run
   ```
3. Commit and merge or push the version bump to `main`:
   ```bash
   git add Cargo.toml Cargo.lock
   git commit -m "chore: bump version to X.Y.Z"
   git push origin main
   ```
4. From a clean, up-to-date `main`, create and push an annotated release tag:
   ```bash
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```
5. Watch the tag-triggered workflows:
   ```bash
   gh run list --workflow Release --limit 5
   gh run list --workflow "Publish to crates.io" --limit 5
   ```
6. `release.yml` builds platform binaries, creates the GitHub Release, and updates the Homebrew formula.
7. `publish.yml` runs `cargo publish` to crates.io automatically.

**Do NOT run `cargo publish` or `gh release create` manually** — the CI workflows handle both.
**Do NOT push the tag before the version bump is on `main`.**
**Do NOT reuse a release tag after a failed publish; bump to a new patch version instead.**

## Non-Goals

- Gemini/Cursor support
- Cost estimation
- Remote/SSH monitoring
- Notifications/alerts

## Terminal Jump (`Enter`)

`Enter` focuses the terminal running the selected session's agent process.
The logic lives in `src/jump/` as a registry of `TerminalJumper` adapters
(one file per backend). `jumpers()` is the single ordered source of truth;
`resolve()` walks it and the first applicable adapter wins.

Each adapter returns a three-way `JumpAttempt`:
- `NotApplicable` — not this backend's terminal; try the next adapter.
- `Jumped` — focused successfully; stop.
- `Failed(msg)` — this backend owns the process but the focus command errored;
  stop and surface `"<backend>: <msg>"` in the status line.

Order (most specific first), mutually exclusive by controlling tty:

1. **cmux** (`jump/cmux.rs`) — reads `CMUX_WORKSPACE_ID` plus optional panel,
   bundled-CLI, and socket variables from the target process via `ps eww`; clears
   unrelated inherited `CMUX_*` values, restores the target socket context, and runs
   `cmux workspace select <uuid>`. On macOS, a broken cmux socket attempts a bounded
   AppleScript workspace/terminal focus fallback before reporting failure.
2. **tmux** (`jump/tmux.rs`) — only when abtop itself runs inside tmux (`$TMUX`).
   Maps PID → pane via `tmux list-panes -a -F '#{pane_pid} #{session_name}:#{window_index}.#{pane_index}'`
   + process-tree descent, then `switch-client` / `select-window` / `select-pane`.
   PID in no pane → `NotApplicable` (lets another backend try).
3. **iTerm2** (`jump/iterm2.rs`) — resolves the PID's controlling tty (`ps -o tty=`),
   then AppleScript selects the session whose `tty` matches and brings its
   window/app to the front. First call triggers a one-time macOS Automation
   permission prompt; until granted, `osascript` exits non-zero → `Failed`.

Parsing, registry, cmux command planning, socket failure, and AppleScript fallback logic
are unit-tested; the thin live `ps`/`osascript`/`tmux` I/O wrappers are verified manually.

## Privacy

abtop reads local session databases, registries, transcripts, prompts, tool inputs, and
memory files for all supported providers. These may contain secrets.

- **`--once` output**: redact file contents from tool inputs. Show only the provider,
  tool name, safe path/location previews, child command previews, and bounded redacted
  summaries; never file contents or raw tool arguments/output.
- **TUI mode**: show the tool name, a safe path/location preview, and a bounded redacted
  summary. The summary may fall back to the first prompt; never show file contents or raw
  tool input/output in the session list. The selected-session detail may render the
  collected, bounded `initial_prompt` field.
- **JSON snapshots**: include bounded, redacted chat and task metadata derived from local
  records. Treat snapshots as private data because project context can remain after
  redaction.
- **Codex plugin hooks**: stream at most 4 MiB without retaining the raw payload;
  materialize only bounded lifecycle fields and discard every prompt, message, tool
  input/output, last-assistant text, raw argv/command, transcript path,
  environment/authentication value, and arbitrary provider string. State may contain
  only schema/helper/install identities, session/turn/tool/subagent IDs, canonical
  event/tool classes, cwd when required, timestamps, exact PID/start incarnations,
  faults/open sets, and at most 128 content-free samples. State is not metadata-free;
  treat cwd and stable IDs as private.
- **Codex state storage**: keep it only below
  `$CODEX_HOME/plugins/data/abtop-abtop-local`, with mode-0700 directories and mode-0600
  files on Unix, same-owner and no-symlink validation, locks, and atomic same-directory
  replacement. The POSIX launcher first precreates a unique private
  `launch-<shell-pid>-pending.<16-alphanumeric-nonce>` marker before abtop starts; 16
  fixed no-clobber names are fallback only. Ingest adopts/enriches the exact inode with a
  fresh random 128-bit commit ID and commits basename plus ID before exact removal. Valid
  commit proofs survive clean generation boundaries; legacy basename-only proofs fail
  closed. Before
  deleting generation state, a writer persists its first GC-side exact-incarnation gone
  confirmation; this is only a deletion grace anchor and never `Done` proof. Removal
  requires a later pass strictly more than 30 seconds afterward plus a fresh gone check.
  Capacity pressure begins that sequence only after terminal state is at least 30 seconds
  old or crashed nonterminal state is strictly older than 24 hours; normal cleanup runs
  on a later `SessionEnd` and requires the strict 24-hour gate for either. The collector
  separately retains already-proven `Done` as a bounded, content-free, non-actionable
  30-second in-memory tombstone. After draining its payload, every later ingest may
  reclaim strictly stale temporary/fixed-slot/ordinary-fault artifacts only from a
  complete, revalidated state snapshot after out-of-lock death probes. Collector reads
  never remove state; persistent overflow remains permanent, monotonic, and fail-closed.
- **Codex setup isolation**: `abtop --setup-codex` may create an absent absolute
  `CODEX_HOME`; write the isolated local marketplace/plugin source bundle and retained
  private `$CODEX_HOME/.abtop-codex-plugin.lock`; create the content-free plugin-data
  attestation, state, and fault tree;
  cause Codex to write the installed cache and native plugin registration; remove exact
  legacy profile blocks; and, on macOS/Linux, create the stable content-free migration
  lock documented above. Setup and uninstall revalidate the stable root lock while held,
  and uninstall retains it so contenders cannot lock different inodes. A source-local
  `abtop/.setup.lock` is legacy cleanup only. Generated launchers embed only the exact
  abtop executable and private data paths, never provider content. It never edits global
  hooks, `notify`, OpenTelemetry, unrelated plugins, Claude configuration, `PATH`, or
  the Codex executable. Migration removes only exact legacy abtop marker blocks; it
  preserves arbitrary aliases, functions, and other profile content. Never bypass hook
  trust or write trusted hashes on the user's behalf.
- **No network**: collectors never send provider session data anywhere and do not require
  API keys or authentication. All collector reads are local.
- **Summary exception**: normal TUI ticks and `--once` pass up to 200 characters each
  from the first user and first assistant text to local `claude --print`, which may call
  Anthropic. The cache stores only generated summaries or sanitized 80-character
  fallbacks. `App::tick_no_summaries()` and `abtop --json` do not launch summary jobs.

## Gotchas

- **Transcript size**: 1KB–18MB. On first load, full scan for totals. After that, track file offset and read only new bytes. Buffer partial lines.
- **Session file deletion**: files disappear when Claude exits. Handle `NotFound` between scan and read.
- **stats-cache.json is stale**: only updated on `/stats` command. Don't use for live data.
- **Context window sources vary**: Claude/Codex use known model limits, Grok reads local signals, and Kimi resolves its local model configuration. Unknown limits must remain unavailable rather than guessed.
- **Rate limit is account-level**: shared across all sessions. Don't show per-session.
- **Path encoding**: `/Users/foo/bar` → `-Users-foo-bar`. Used for transcript directory names.
- **Path encoding collision**: `-Users-foo-bar-baz` could be `/Users/foo/bar-baz` or `/Users/foo-bar/baz`. Use session JSON's `cwd` as source of truth.
- **lsof can be slow**: on macOS with many open files. Cache results, poll every 10s.
- **Child process tree**: `pgrep -P` only gets direct children. Build full tree from `ps -eo ppid`.
- **Port detection race**: a port can close between lsof and display. Show stale data gracefully.
- **Subagent directory may not exist**: only created when Agent tool is used. Check existence before scanning.
- **Undocumented internals**: all five providers' local data sources are implementation details, not stable APIs. Schemas may change without notice. Parse defensively, ignore unknown records, and bound untrusted strings/collections.
- **Codex hook compatibility**: setup supports only exact `codex-cli` 0.146.0 and must
  validate exact `hooks stable true` / `plugins stable true` feature rows plus exactly
  the supported 11 uppercase `ManagedHooksRequirements` properties in generated
  `v2/ConfigRequirementsReadResponse.json`. Runtime evidence additionally requires the
  exact source bundle, Codex's installed cached copy, attestation, and base hook state.
  This is not an effective live-thread hook-engine attestation. Profiles, SessionFlags,
  project/config-lock layers, managed/cloud policy, per-thread state, and live reload can
  differ. Codex 0.146.0 cannot attest those effective layers, so every live lifecycle
  candidate remains non-actionable `Unknown`; only independently exact transition-bound
  `Done` may promote. It must not interfere with native Codex launch. Secure setup/runtime
  are macOS/Linux-only; Windows Codex remains unmanaged `Unknown`. `abtop --setup` is
  Claude-only.
- **Codex trust review**: setup never approves hooks or bypasses trust. A schema/helper
  identity change must alter the declared command and cause Codex to request review
  again. A changed helper path or byte content changes that identity; tell the user to
  rerun setup and restart Codex after every abtop update or replacement, and approve only
  `abtop@abtop-local` whenever Codex presents a fresh review.
- **Legacy wrapper migration**: remove only one structurally valid pair of the exact
  abtop markers. Missing markers are success; duplicates or unmatched markers fail
  closed. An already-loaded legacy function can persist until the shell exits, so 0.6's
  hidden trampoline must directly delegate all native arguments, streams, and exit
  behavior with no relay or allowlist. Remove that trampoline in 0.7.
- **Codex state validation**: accept only private, supported-schema hook state with a
  matching helper/install identity and exact PID/start incarnation. Before helper exec,
  the POSIX launcher first creates a unique empty private marker and falls back to 16
  fixed no-clobber names only if unique allocation fails. The helper anchors, adopts, and
  enriches the exact marker with a fresh random 128-bit commit ID before validation,
  commits basename plus ID with the fold, then removes only that inode. Preserve valid
  commit proofs across clean generation boundaries and reject legacy basename-only
  proofs. Missing/rejected tokens use a generic marker; exhaustion leaves persistent
  overflow. A failed launch, timeout, crash, malformed
  event, stale/unsafe state, or incomplete lifecycle poisons affected state instead of
  leaving stale positive evidence. Cleanup must obey confirmed-process and
  retention/capacity rules. Treat `session_id` as the shared root and `agent_id` as a
  child identity. Never fall back to time, CPU, mtime, or uncorrelated child activity.
- **Codex status ceiling**: live hook evidence is `Unknown / Unavailable`; local rollouts
  remain useful for tokens, context, rate limits, metadata, and exact edge correlation,
  but never promote `Thinking`, `Executing`, or `Idle`. Only an independently exact
  supported process/session `Live → Gone` transition may produce non-actionable
  `Done / Heuristic` for 30 seconds. Approval/question candidates and root/child open
  tools remain `Unknown`, not `Waiting` or stale `Executing`.
- **Grok shared PID**: a single Grok TUI can own multiple session rows. Attribute memory, children, and ports once; a kill affects every session on that PID.
- **Kimi same-cwd ambiguity**: several live Kimi processes sharing one root+cwd cannot be mapped authoritatively. Keep those rows `Unknown` and unkillable.
- **Kimi process title**: current Kimi Code replaces argv with the bare `kimi-code` title. Host-mode exclusions are best-effort when original arguments survive; ownership still requires explicit or post-start activity evidence.
- **Custom Grok/Kimi homes**: process-environment inspection is best-effort and platform-dependent. Launch abtop with the same `GROK_HOME` / `KIMI_CODE_HOME` when inspection is unavailable.
- **Legacy Kimi CLI**: `~/.kimi` from the retired `MoonshotAI/kimi-cli` project is not scanned. Only current Kimi Code data under `~/.kimi-code` is supported.
- **Terminal size**: minimum 60×18. Widths below 100 use the tabbed narrow layout;
  desktop height pressure prioritizes Sessions and omits Context first.
- **PID reuse in port cache**: invalidate cached ports when the set of tracked PIDs changes.
- **Rate limit staleness**: reject rate limit data older than 10 minutes.
- **`/clear` + multi-PID same cwd**: after `/clear`, Claude Code mints a new `sessionId` + `.jsonl` without rewriting `sessions/{PID}.json`. abtop overrides the stale sid by picking the newest transcript in the project dir, but this heuristic can't disambiguate ownership when two live `claude` PIDs share a cwd — so the override is disabled in that case and both sessions keep their original sid until exit. Use separate worktrees if live tracking is needed on both simultaneously.
