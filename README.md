# abtop

**Like [btop](https://github.com/aristocratos/btop), but for your AI coding agents.**

See Claude Code, Codex CLI, OpenCode, Grok, and Kimi Code sessions at a glance — token
usage, context window %, rate limits, child processes, open ports, and more.
Sessions are discovered from local process and file state across macOS, Linux, and Windows.

![demo](https://raw.githubusercontent.com/graykode/abtop/main/assets/demo.gif)

## Why

- Running 3+ agents across projects? See them all in one screen.
- Hitting rate limits? Watch your quota in real-time.
- Agent spawned a server and forgot to kill it? Orphan port detection.
- Context window filling up? Per-session % bars with warnings.

## Contents

- [Install](#install)
- [Quick Start](#quick-start)
- [Command Reference](#command-reference)
- [Codex Hook Integration](#codex-hook-integration)
- [Interface](#interface)
- [Supported Agents](#supported-agents)
- [Status and Evidence](#status-and-evidence)
- [Themes](#themes)
- [Configuration](#configuration)
- [Key Bindings](#key-bindings)
- [Library / JSON snapshot](#library--json-snapshot)
- [Privacy](#privacy)

Collection reads local state and needs no API keys. The disabled-by-default CodexBar
quota integration may use CodexBar's configured automatic local, web, OAuth, or API
sources when explicitly enabled.
The normal TUI and `--once` can invoke the installed Claude CLI to generate session
titles; see [Privacy](#privacy) for the exact data boundary. Optional Codex hook
integration records only bounded, content-free lifecycle metadata. It never participates
in the interactive Codex launch path: plain `codex ...` remains native. Setup invokes
bounded native compatibility-preflight and plugin-administration commands. Legacy zsh
inspection can also run bounded login and non-login probes so an unexported `ZDOTDIR`
is not missed. The native plugin integration and normal launch path never wrap, replace,
alias, or proxy Codex, and abtop never inspects provider credentials. A narrowly scoped
0.6 compatibility trampoline for an already-loaded retired wrapper is documented below.

## Install

### macOS / Linux

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/graykode/abtop/releases/latest/download/abtop-installer.sh | sh
```

### Cargo

Building or installing from source requires Rust 1.88 or newer.

```bash
cargo install abtop
```

### Windows

Native support — no WSL required. Uses `sysinfo` for process info and host CPU/MEM
metrics, and `netstat -ano` for listening ports. Windows has no load average, so LOAD is
reported as 0. OpenCode session discovery additionally requires the `sqlite3` CLI
(`winget install SQLite.SQLite`); without it abtop prints a one-time warning to stderr.

```powershell
powershell -c "irm https://github.com/graykode/abtop/releases/latest/download/abtop-installer.ps1 | iex"
```

Or run `cargo install abtop` from a terminal with the Rust toolchain available. Claude
Code config is resolved automatically from `%USERPROFILE%\.claude`.

### Other

Pre-built binaries for all platforms are available on the
[GitHub Releases](https://github.com/graykode/abtop/releases) page.

### Optional dependencies

The base monitor starts without provider credentials or account setup. Extra local
programs enable the following features:

| Feature | Requirement |
| ------- | ----------- |
| OpenCode sessions | A `sqlite3` CLI on `PATH` with `-readonly` and `-json` support |
| Claude account quota | `bash` and `python3` in Claude Code's hook environment |
| Generated session titles | An installed `claude` CLI; the normal TUI and `--once` may invoke `claude --print` |
| Codex lifecycle audit and exit correlation (macOS/Linux) | Stable native `codex-cli` 0.145.0 or newer with enabled `hooks` and `plugins` features and all 11 required generated hook events |
| Optional CodexBar quotas | A `codexbar` CLI on `PATH`; collection remains disabled until explicitly enabled |
| Project dirty-file counts | `git` on `PATH` |
| Full Unix process, open-file, port, and MCP discovery | `ps` and `lsof` |
| `abtop --update` | `curl` and `sh` |

Missing optional dependencies degrade only their corresponding feature. For example,
abtop can still monitor non-OpenCode sessions without `sqlite3`.

## Quick Start

Launch the monitor immediately after installation:

```bash
abtop
```

No setup is required for ordinary local session discovery. Two optional setup commands
enable data that providers do not otherwise expose reliably.

### Claude account quota

```bash
abtop --setup
```

This installs a Claude Code StatusLine hook in the active config root:
`CLAUDE_CONFIG_DIR` when it is valid UTF-8 and names an existing directory, otherwise
`~/.claude`. It writes `abtop-statusline.sh` and registers it in `settings.json`; the
hook later produces `abtop-rate-limits.json` when a Claude response supplies
`rate_limits`. If `statusLine.command` is already a different nonempty string, setup
exits instead of replacing that setting. Restart any running Claude Code sessions after
setup. Quota appears after the next response that includes quota data and remains
unavailable when the provider does not supply it.

`abtop --setup` is Claude-only. It does not inspect or change Codex configuration.

### Optional CodexBar quotas

Enable **CodexBar quotas** in the `c` configuration overlay to show quota data for all
providers enabled in CodexBar, or set the existing compatibility key:

```toml
codexbar_quota_fallback = true
```

This opt-in integration runs CodexBar's normal configured usage command with bounded
JSON output and a timeout. It honors CodexBar's enabled providers and automatic source
selection, which may use existing local, authenticated web, OAuth, or API access. abtop
does not select accounts or inspect credentials. It retains only bounded quota-window
metadata and sanitized diagnostics; account email, organization, credits, pace
summaries, dashboard data, raw errors, and arbitrary provider text are discarded.

Every returned primary, secondary, tertiary, and extra quota window is eligible for
display. Fresh native Claude and Codex windows win for overlapping slots; CodexBar
fills missing or stale native windows and contributes every non-overlapping window.
When a plan shifts a standard window between built-in slot names, exact duration and
reset metadata prevents that same limit from appearing twice; custom windows remain
distinct.
One provider failure does not discard successful providers, and a sanitized unavailable
card identifies an enabled provider whose CodexBar source failed.

The Quota panel uses an adaptive provider grid, marks CodexBar-only data with `·CB`
and mixed native/CodexBar data with `·MIX`, and reports explicit overflow when all
windows cannot fit. `--once` and `--json` retain the complete bounded provider/window
set. The configuration overlay reports `off`, `checking`, `active`, `partial`, or
`unavailable`; `abtop --json` exposes provider state, provenance, freshness, and fixed
sanitized error categories under `codexbar_quota`. Poll failures are non-fatal, and
CodexBar values remain in memory rather than being written into native quota caches.

### Codex lifecycle audit and exit correlation

The integration records content-free lifecycle boundaries for auditing and exact
process-exit correlation. Supported Codex releases cannot attest the effective hook engine of a
live thread. Exact supported hook/rollout shapes may nevertheless provide conservative,
non-actionable heuristic `Thinking`, `Executing`, or `Idle`; exactly correlated Herdr
terminal state may additionally provide `Waiting`, `Idle`, refined work, or generic
`Working` when activity is exact but its kind cannot be refined safely.

Native hook integration is currently supported on macOS and Linux. Windows still gets
Codex process, rollout, token, context, and quota metadata, but its live lifecycle status
remains `Unknown`; secure hook setup fails before changing Codex configuration there.

Install abtop's isolated local Codex plugin for the current abtop executable:

```bash
abtop --setup-codex
```

Setup uses the native `codex plugin` commands to register and enable
`abtop@abtop-local` in the current `${CODEX_HOME:-~/.codex}`. It does not edit `PATH`,
define a shell function or alias, replace the Codex executable, or change command-line
arguments. Existing global hooks, `notify`, plugins, and OpenTelemetry configuration
remain untouched. Normal Codex commands keep their native behavior:

```bash
codex
codex resume
codex fork
codex --yolo
```

Setup supports stable `codex-cli` 0.145.0 and newer releases. It also requires the native feature list to
contain the exact `hooks stable true` and `plugins stable true` rows, and verifies that
the uppercase `ManagedHooksRequirements.properties` in generated
`v2/ConfigRequirementsReadResponse.json` contain all 11 required events. Additional
events are allowed; older releases or releases missing a required capability fail closed;
if plugin installation cannot complete, setup attempts a lost-update-safe rollback of
legacy profile edits and preserves any concurrent editor save rather than overwriting it.
Run setup again after updating Codex so the selected release's features and generated
schema are checked before starting a fresh native Codex session.

Restart Codex after setup. If Codex asks for a trust review, approve only the 11 hooks
attributed to `abtop@abtop-local`; setup never writes trusted hook hashes itself. A
successful setup can therefore exit 0 while review is still required, whereas
`abtop --codex-integration-status` remains exit 1 / `not ready` until the base config
trusts and enables all 11 exact hooks. Run
`abtop --codex-integration-status` to audit the installation. See
[Codex Hook Integration](#codex-hook-integration) for its status and privacy limits.
After replacing or updating the abtop binary, run `abtop --setup-codex` again: the exact
helper digest is part of the hook identity, so old integration state deliberately becomes
unavailable for hook-refined status until the new plugin copy is installed, reviewed, and
loaded by a fresh Codex session. An independently exact Herdr terminal may still report
generic `Working`, `Waiting`, or `Idle` during that interval.

## Command Reference

| Command | Behavior |
| ------- | -------- |
| `abtop` | Launch the interactive monitor. |
| `abtop --once` | Print one human-readable snapshot. It may wait up to 30 seconds for missing titles generated by `claude --print`. |
| `abtop --json` | Print one machine-readable JSON snapshot without spawning summary jobs. |
| `abtop --theme <name>` | Override the theme for this launch. Use the TUI to persist a choice. |
| `abtop --mouse` | Enable mouse click and wheel handling; mouse capture is otherwise off. |
| `abtop --demo` | Show deterministic demo data with default panels and theme (unless `--theme` is supplied); persisted discovery/visibility settings are ignored, as are keyboard `r`, `x`, `X`, and `Enter` actions. |
| `abtop --exit-on-jump` | Exit abtop after a successful `Enter` terminal jump. |
| `abtop --setup` | Install the Claude quota hook. It does not change Codex configuration. |
| `abtop --setup-codex` | On macOS/Linux, remove exact legacy abtop Codex wrapper blocks, then install or repair the isolated `abtop@abtop-local` Codex plugin. |
| `abtop --uninstall-codex` | Remove abtop's local Codex plugin integration and exact retired wrapper blocks, preserve its content-free audit data, and leave unrelated configuration unchanged. |
| `abtop --codex-integration-status` | Audit native compatibility, the local plugin source and installed cache, declaration, helper, base trust/enablement, and retired-wrapper cleanup. It reports not ready on unsupported platforms. |
| `abtop --version`, `abtop -V` | Print the installed abtop version. |
| `abtop --update` | Download and run the latest release shell installer using `curl` and `sh`. |

`--once` can wait for title generation before printing, while `--json` performs a single
summary-free collection pass and is the safer interface for scripts and local tools.

The three Codex administration commands are exact singleton invocations. They return
exit code 0 on success, exit code 1 when setup/uninstall fails or integration status is
not ready, and exit code 2 for invalid command-line usage.

The repository also includes a fail-closed agent-tui integration harness:

```bash
scripts/agent-tui-e2e.sh \
  --suite codexbar \
  --abtop "$PWD/target/release/abtop"
```

The deterministic `codexbar` suite uses an empty isolated Codex home, a fixed mixed
multi-provider response (including a sanitized provider failure), private agent-tui
state, and a Quota-only abtop configuration. The
`codex-status` suite additionally requires a healthy, already reviewed hook integration
and an already trusted Git workspace; it starts two bounded authenticated Codex turns in an
exact disposable Herdr session and validates `Idle → Exec → Idle` plus Enter-to-focus.
It never approves hooks or touches Herdr's default session. Controlled artifacts are
retained with private permissions at the path printed on exit.

Integration status audits the exact bundle and trust/enablement recorded in base
`$CODEX_HOME/config.toml`; it is not an attestation of a live thread's in-memory hook
engine. Profiles, command-line or per-thread overrides, project/config-lock layers,
managed/cloud policy, and live reload can differ. Supported Codex releases expose no
thread/PID/generation-bound proof of the effective hook engine, so a healthy setup or
status result is installation readiness only. Exact supported lifecycle shapes may
display non-actionable heuristic `Think`, `Exec`, or `Idle`, but do not authorize PID
actions. Herdr terminal evidence is independently correlated and does not upgrade the
plugin audit. Only the validated process-exit transition described below reports
heuristic `Done`.

## Codex Hook Integration

On macOS and Linux, `abtop --setup-codex` creates a private local marketplace and plugin
below the active `${CODEX_HOME:-~/.codex}`:

```text
$CODEX_HOME/abtop/marketplace/
├── .agents/plugins/marketplace.json
└── plugins/abtop/
    ├── .codex-plugin/plugin.json
    ├── hooks/hooks.json
    └── scripts/abtop-codex-hook.{sh,cmd}
```

Before committing the integration, setup requires stable `codex-cli` 0.145.0 or newer,
exact `hooks stable true` and `plugins stable true` feature rows, and all events from
the required 11-event contract in
`v2/ConfigRequirementsReadResponse.json`'s uppercase
`ManagedHooksRequirements.properties`. It then registers the marketplace and plugin by
invoking the exact lexical `codex` entry selected from `PATH` whose execution proved that
version and contract. This preserves argv-sensitive shims such as mise rather than
silently substituting their canonical target. Setup verifies that the plugin is installed
and enabled. Runtime
state is stored in `$CODEX_HOME/plugins/data/abtop-abtop-local`. Re-running setup repairs
or updates the bundle. A changed abtop executable path or byte content changes the helper
digest; even a byte-identical replacement must be followed by setup, so run setup again
after every abtop update or replacement and restart Codex. The plugin version and hook command
include the schema revision and helper identity digest, causing Codex to request a new
trust review instead of silently reusing an old approval. Hook evidence is accepted only
when the private source bundle, Codex's installed cached copy, install attestation, and
base trust/enablement state match the current declaration exactly. Those checks establish
installation integrity, not complete effective per-thread hook coverage.

Setup and uninstall serialize owned plugin mutations with the stable
`$CODEX_HOME/.abtop-codex-plugin.lock`. On Unix it is a same-owner regular file with
mode `0600`, is revalidated while held, and is retained after uninstall so concurrent
administrative processes cannot lock different replacement inodes. A source-local
`$CODEX_HOME/abtop/.setup.lock` belongs only to older installations and is removed as
legacy source-bundle debris; it is not the current lock.

Compatibility preflight briefly invokes `codex app-server generate-json-schema` to
inspect that local release's schema. This subprocess is not a relay, supervisor,
monitoring transport, or persistent daemon, and abtop never attaches to a Codex daemon.
Every native Codex administrative command receives null stdin, has a 15-second overall
timeout, and captures at most 1 MiB from each of stdout and stderr. On Unix it runs in
its own process group with nonblocking pipe drains; abtop terminates that group on
timeout or when descendants retain an output pipe for 100 ms after the leader exits,
then reaps only for a bounded interval. On Windows the portable path uses bounded
reader channels and a kill-on-close Job Object when assignment succeeds, with direct
child termination as a fallback. Mutating plugin commands are bracketed by checks of
the exact selected Codex executable identity and fail if it changes.

The plugin subscribes, without matchers, to the same 11-event set validated for every
supported Codex release:

| Session and turn | Tools and interaction | Subagents and compaction |
| ---------------- | --------------------- | ------------------------ |
| `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `Stop` | `PreToolUse`, `PermissionRequest`, `PostToolUse` | `SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact` |

Each hook is synchronous, silent, and limited to one second, so it can delay the
corresponding Codex edge by at most that configured timeout. The helper parses at most
4 MiB as a stream, materializes only bounded lifecycle fields, discards every other JSON
value, and attempts to drain malformed input. The launcher and helper absorb all errors
and produce no stdout or stderr. After its bounded dispatch attempt, the launcher always
exits with code zero regardless of helper success, failure, or absence. Monitoring
therefore fails open for Codex itself: an unavailable or outdated abtop reduces
monitoring confidence but cannot deny or alter the agent action.

Setup does not add `PostToolUseFailure`; additional events advertised by newer Codex
releases do not expand abtop's installed hook set automatically. Setup also leaves existing user hooks, `notify`, OpenTelemetry, and
other plugins unchanged. Do not bypass hook trust globally. After setup, restart plain
native Codex and review only the 11 hooks shown for `abtop@abtop-local`. Only sessions
started after that restart use the currently installed and reviewed helper identity;
older live sessions retain untrusted integration state and cannot supply hook-refined
status.

No shell integration is installed. Setup removes only the exact legacy blocks delimited
by `# >>> abtop managed codex >>>` and `# <<< abtop managed codex <<<`; missing blocks
are already clean, while malformed or duplicate markers fail closed without editing the
file. Replacements are locked, revalidated, atomic, and rolled back safely if migration
cannot complete on macOS/Linux. Unrelated aliases, functions, and profile contents are
preserved.
Zsh, bash, and fish are migrated automatically when applicable. To discover an
unexported custom `ZDOTDIR`, setup, uninstall, and integration-status inspection can run
strictly framed, output-bounded login and non-login zsh probes; zsh evaluates its normal
startup files, but abtop persists none of their content. Windows setup is unsupported
before migration; Windows uninstall can instead return exact manual PowerShell cleanup
guidance. If an old wrapper function is still loaded in the current shell, start a fresh
shell after setup. On macOS/Linux, migration and integration-status inspection share the
stable private mode-`0600` lock `~/.abtop-codex-migration.lock`. The file is created on
the first inspection and intentionally retained so concurrent processes never lock
different replacement inodes; it contains no provider or shell-profile content. The
non-Unix lock implementation is a no-op.

Uninstall applies the same exact-marker migration and preserves the bounded content-free
state in `$CODEX_HOME/plugins/data/abtop-abtop-local` for audit. As a recovery rule it
always asks native Codex to remove the reserved `abtop@abtop-local` plugin ID first,
including when the marketplace record is missing, malformed, or conflicting; do not use
that reserved ID for an unrelated plugin. It removes the marketplace registration and
source bundle only after proving they point to abtop's exact local source, otherwise it
preserves them and exits with manual-recovery guidance. Other profile content and Codex
configuration remain unchanged. Unlike setup and healthy-status validation, uninstall
accepts any exact stable `X.Y.Z` Codex semver so a downgrade or upgrade cannot strand the
integration without a recovery path.

Release 0.6 retains `abtop codex -- ...` only as a hidden compatibility trampoline for
an already-loaded legacy wrapper. It requires that wrapper's exact captured Codex path
and directly delegates arguments, standard streams, and exit status to the native
executable; on Unix, process replacement also preserves native signal behavior. It is
not a monitoring launcher, has no argument allowlist, and is
scheduled for removal in 0.7. New scripts and documentation must always invoke
`codex ...` directly.

## Interface

The numbered panels can be toggled with `1`–`7`; their visibility is persisted.
For orientation, press `?` for help, `/` to filter sessions, `c` for configuration,
`Enter` to jump through exact Herdr session identity or actionable PID ownership, and
`x` twice to confirm a session kill. See
[Key Bindings](#key-bindings) for the complete controls.

| Panel | Contents |
| ----- | -------- |
| **1 Context** | Token-rate history and per-session context-window gauges when reliable window data exists. |
| **2 Quota** | Account-level quota windows and reset times from native Claude/Codex sources plus every provider enabled through the optional CodexBar integration. |
| **3 Tokens** | Input, output, cache, turn, and selected-session token history. |
| **4 Projects** | Project branch and dirty-file counts collected through `git`. |
| **5 Ports** | Agent child listeners, conflicts, and ports orphaned after their parent session exits. |
| **6 Sessions** | Session list, current status/task, selected-session evidence, children, subagents, timeline, and file audit. |
| **7 MCP** | Detected Codex `mcp-server` processes, profiles, rollout counts, and recent activity. A rollout updated within 30 minutes counts as active. |

MCP-owned rollouts are suppressed from Sessions by default to prevent duplicate or
ghost rows. `M` changes that behavior for the current run only.

The supported minimum terminal size is **60x18**; smaller terminals show a size warning
when the pane is tall enough to render it.
Widths from 60 through 99 use a tabbed **Work / Usage / System** layout, while widths of
100 or more use the desktop layout. In desktop mode, Sessions receive priority when
height is constrained and the Context panel is omitted first; narrow-mode sections
split the available height equally unless one is maximized. **120x40** or larger remains
recommended.

Mouse capture is off by default so drag selection and copy continue to work. Launch with
`--mouse` to enable panel, tab, session, zoom, and orphan-port click targets plus wheel
navigation.

### Terminal Jump

Press `Enter` to focus the selected agent terminal. abtop supports
Herdr 0.7.0 or newer, cmux, tmux, and iTerm2 on macOS. When abtop runs inside
Herdr, it can jump to agents in any pane, tab, or workspace in the same Herdr
session; no additional setup is required. Unsupported environments do nothing. iTerm2
can request macOS Automation permission on first use. Herdr first matches one exact
native provider/session reference, so an exact row can be focused even when its
lifecycle status is `Unknown` or `Done`. When no exact reference exists, all backends
retain the existing actionable PID and fresh process-incarnation requirements.

```bash
tmux new -s work
# pane 0: abtop
# pane 1: claude (project A)
# pane 2: claude (project B)
# → Enter on a session in abtop jumps to its pane
```

The same flow works inside Herdr: run abtop in one pane, run agents elsewhere
in the same session, then press `Enter` on the selected abtop row.

## Supported Agents

✅ means available, ⚠ means conditional or deliberately limited, and ❌ means
unavailable. Provider-specific caveats and evidence authorities are explained in
[Status and Evidence](#status-and-evidence).

| Feature | Claude Code | Codex CLI | OpenCode | Grok | Kimi Code |
| ------- | :---------: | :-------: | :------: | :--: | :-------: |
| Session discovery | ✅ | ✅ | ✅ | ✅ | ✅ |
| Token tracking | ✅ | ✅ | ✅ | ✅ | ✅ |
| Context window % | ✅ | ✅ | ⚠ estimated | ⚠ local data | ⚠ local config |
| Status detection | ⚠ mixed evidence | ⚠ conservative heuristics | ⚠ limited | ✅ | ⚠ mixed evidence |
| Current task | ✅ | ⚠ generic/refined | ⚠ generic | ✅ | ✅ |
| Account quota | ✅ | ✅ | ❌ | ⚠ CodexBar | ⚠ CodexBar |
| Git status | ✅ | ✅ | ✅ | ✅ | ✅ |
| Children / ports | ✅ | ✅ | ✅ | ✅ | ✅ |
| Subagents | ✅ | ✅ | ❌ | ✅ | ✅ |
| Memory status | ✅ | ❌ | ❌ | ❌ | ❌ |

## Status and Evidence

Status answers what the agent is doing; evidence answers how confidently abtop can prove
it. UI labels and their JSON enum values are:

| UI | JSON | Meaning |
| -- | ---- | ------- |
| `◉ Think` | `Thinking` | A model turn is open and no tool is currently running. |
| `● Exec` | `Executing` | A tool, task, subagent, background terminal, or verified active child is working. |
| `◐ Work` | `Working` | Exact terminal evidence proves activity, but its kind cannot be classified safely. |
| `◌ Wait` | `Waiting` | An explicit unresolved approval or question requires user action. It wins over concurrent background work. |
| `○ Idle` | `Idle` | The live session has no active model, tool, task, or interaction work. |
| `? Unknown` | `Unknown` | Ownership or lifecycle proof is missing, stale, malformed, disconnected, contradictory, or otherwise insufficient. |
| `⏳ Rate` | `RateLimited` | The provider reports a current rate-limit block. A quota percentage alone never sets this lifecycle status. |
| `✗ Error` | `Error` | The provider reports a current fatal session or turn failure. Raw provider error content is not used as the task label. |
| `✓ Done` | `Done` | A verified process exit has been observed. |

After ownership and lifecycle validation, precedence is `Waiting` > `RateLimited` >
`Error` > `Executing` > `Thinking` > `Working` > `Idle`. `Working` is used only when
exact activity cannot be refined to `Thinking` or `Executing`. abtop never turns elapsed
time, low CPU, or an old transcript timestamp into `Wait`. `Unknown` is the fail-closed
result when that precedence cannot be applied safely; `Done` is terminal exit evidence.

Each row also carries one of these evidence authorities:

| Authority | Meaning |
| --------- | ------- |
| `Provider` | Exact provider lifecycle data. Codex hook evidence is never promoted to this authority. |
| `Heuristic` | A local-file, process, or exactly correlated terminal inference that is useful but not provider-authoritative. |
| `Unavailable` | No sufficiently reliable current source exists. |

The evidence record includes a machine-readable reason, observation and status-since
timestamps, connection generation, consecutive matching count, and bounded sample
history. `observed_at_ms` is the time of the newest evidence sample; its displayed
freshness is the age of that observation. `status_since_ms` is when the current status
began. `connection_generation` identifies a protocol connection generation; zero is
used for hook and other non-protocol evidence and is displayed as `—`. Consecutive matching
counts samples with the same status, authority, and connection generation. `--once`
prints the current evidence plus the latest five status samples. The selected-session
detail shows the same line when terminal width permits; `--json` always includes the
complete current fields and latest five content-free samples. Fields ending in `_ms`
use Unix epoch milliseconds.

Process actions fail closed too. `Working`, `Unknown`, and `Done` rows, `Unavailable` evidence, a
missing exact process identity, or a failed fresh PID/provider revalidation disables
kill and PID-based terminal jump. An exact Herdr native session reference is separate
focus-only authority and never enables kill. Kimi is stricter for PID actions: even a
non-`Unknown` heuristic row is non-actionable; its process actions require `Provider`
authority. The first `x` records
an exact-session confirmation and a matching second press within two seconds performs
the kill. A Grok confirmation reports how many logical sessions share the target PID.

Codex `Done` is transition-bound. This collector instance must first observe an exact
live PID/start ↔ supported-version rollout-tree binding, then observe that same process
incarnation become gone. It anchors the 30-second window at that transition and retains
a bounded, content-free, non-actionable in-memory tombstone through the exact boundary,
even if the source state later disappears. A temporarily unavailable hook-state scan
may preserve an existing tombstone but cannot create one; a fresh collector that first
sees an already-gone process, or sees only a reused numeric PID, cannot fabricate
`Done`. Once an exact process scan confirms it is gone, a hook-only generation without
that observed transition is omitted instead of remaining as a PID-zero `Unknown` row or
being relabeled `Done`. Other providers can disappear immediately after verified exit.
Historical Codex rollouts never create PID-zero `Done` rows.

### Claude Code

Claude discovery maps live processes to local config roots and transcripts. Recognized
native registry states and durable transcript, tool, and subagent lifecycle can provide
`Provider` evidence; activity inferred only from descendant processes is `Heuristic`.
An unorderable native/transcript disagreement, or incomplete subagent lifecycle that
would otherwise look idle, fails closed to `Unknown`. Account quota requires the
[Claude setup](#claude-account-quota) hook.

Quota values older than ten minutes remain visible but are dimmed and omit their reset
countdown. The panel has one Claude provider card: if several Claude roots contain quota
files, the newest complete valid native sample is used as one account; equally dated or
undated samples preserve discovery order, with `~/.claude` checked before the environment
and additional roots. When CodexBar quotas are enabled, fresh native Claude windows retain
precedence over overlapping CodexBar windows.

### Codex CLI

Codex combines the content-free events from [Codex Hook
Integration](#codex-hook-integration) with local rollout data. Hooks establish lifecycle
edges; rollouts supply session identity, project/model metadata, tokens, context, quota,
summaries, and subagent relationships. Rollout ordering, mtime, token activity, CPU use,
child activity, and incomplete tool records never establish live status by themselves.

Supported Codex releases expose no thread/PID/generation-bound attestation of the effective hook
engine after profile, project/config-lock, managed/cloud, command-line, per-thread, and
live-reload layers are applied. Base trust, enablement, bundle integrity, individual hook
events, and rollout correlation therefore cannot prove complete live coverage. Exact,
supported process/session/hook/rollout shapes may still supply conservative
`Heuristic` display states, but every live Codex row remains non-actionable.

When Codex runs inside Herdr, abtop can supplement those shapes with Herdr's private
terminal detector. It accepts only a stable `herdr:codex` session-ID match bracketed
around `pane process-info`, with the exact native Codex PID and process incarnation.
Herdr `blocked` reports `Waiting`, Herdr `idle` reports `Idle`, and Herdr `working`
reports `Thinking` or `Executing` only when exact Codex lifecycle evidence classifies
the work. Otherwise the same exact, stable activity reports generic
`Working / Heuristic` with reason `HerdrWorkingUnrefined`. Missing Herdr, failed snapshot
bracketing, duplicate matches, pane movement, PID reuse, malformed output, and timeouts
remain `Unknown`; terminal titles and screen content are never ingested.

The independent exit proof still requires exact process-incarnation, session,
configuration, and event correlation. The matched process-owned root rollout must report
a stable `cli_version` of 0.145.0 or newer, and every discovered descendant rollout must
report that same version. Missing, older, malformed, child-only, or descendant-mismatched version metadata
cannot seed exit proof. abtop applies this deliberately strict matrix:

| Evidence | Codex status |
| -------- | ------------ |
| A previously observed exact live PID/start ↔ supported-version rollout-tree binding, followed by that same process incarnation changing from live to gone | `Done / Heuristic` for 30 seconds |
| Exact supported root turn with no tool or active child | `Thinking / Heuristic`, non-actionable |
| Exact supported active direct-child model work with a complete child set | `Executing / Heuristic`, non-actionable |
| Exact supported stop plus matching rollout completion and a clean tree | `Idle / Heuristic`, non-actionable |
| Stable Herdr `blocked` or `idle` with exact session/PID/incarnation correlation | `Waiting / Heuristic` or `Idle / Heuristic`, non-actionable |
| Stable Herdr `working` plus exact root-turn, root-tool, or direct-child evidence | `Thinking / Heuristic` or `Executing / Heuristic`, non-actionable |
| Stable Herdr `working` with exact session/PID/incarnation correlation but no safe lifecycle refinement | `Working / Heuristic` with `HerdrWorkingUnrefined`, non-actionable |
| A root `PreToolUse`/open rollout tool, any child with an open tool, `PermissionRequest`, or a `request_user_input` candidate | `Unknown / Unavailable`; Codex may be executing or waiting for approval |
| Any `SessionStart`, including startup, resume, clear, and compact | Generation evidence only; never sufficient for `Idle` |
| Rollout `stream_error`/`error`, failed `task_complete`, an unparseable open descriptor, or a nonterminal extra root tree | `Unknown / Unavailable`; invalid rollout lifecycle cannot seed new exit proof |
| A child `PreToolUse`/open tool or any direct-child active/terminal/provisional mismatch | `Unknown / Unavailable`; incomplete child lifecycle never proves live work or rest |
| Missing/different root or descendant `cli_version`; delayed, aborted, stale, duplicate, mismatched, or unsupported active/non-direct child lifecycle; missing/out-of-order hooks; uncovered or hosted tools; a relevant process descendant when root inactivity is required; malformed/stale state; configuration drift; or ambiguous ownership | `Unknown / Unavailable` |
| A hook-only generation whose exact process incarnation is confirmed gone without this collector having observed the required live-to-gone transition | Omitted; never retained as PID-zero `Unknown` and never fabricated as `Done` |

Codex queues startup, resume, and clear `SessionStart` hooks into the next turn,
immediately before `UserPromptSubmit`; those sources reset to a clean generation. A
compact `SessionStart` follows `PostCompact` inside the current turn and preserves active
work. Every source proves a lifecycle boundary, not the absence of work, and none can
become `Idle` on its own. A newly opened empty composer may emit no hook evidence and
remains `Unknown`.

`Stop` and `SubagentStop` are provisional candidates: another hook may block a stop, and
the same root or child can continue in the same turn. Later matching activity reopens
that actor. A provisional child stop closes only when the exact child rollout later
reaches `task_complete`; continued child model work is promoted only from a complete,
exact direct-child set. Neither `SessionStart` nor `Stop` alone proves `Idle`.

Supported Codex releases do not expose enough prompt-display and resolution lifecycle to
distinguish approval/question waits safely. Therefore those states are intentionally
`Unknown`, never guessed as `Wait`, `Exec`, or `Idle`: observing a
`PermissionRequest` or question candidate does not prove when the displayed interaction
is resolved. Hook data also does not produce live `Error` or `RateLimited`; Codex
rate-limit records remain quota metadata only. A root open tool also remains `Unknown`:
Codex can run `PreToolUse` before a separately configurable `PermissionRequest`, so an
open hook/rollout call does not prove that execution has begun. abtop never substitutes
elapsed time, low CPU, or file mtime for a missing event.

Codex uses one root `session_id` for the root and every descendant hook. A child
`agent_id` identifies subagent lifecycle within that shared root; abtop folds those
records into one root state and never treats `agent_id` as a separate Codex session.

Before abtop starts, the POSIX launcher first uses `mktemp` to exclusively create an
empty mode-`0600` marker named
`launch-<shell-pid>-pending.<16-alphanumeric-nonce>` in the embedded private fault
directory. Only if that unique allocation fails does it try the 16 fixed no-clobber
`launch-<slot>-abtopv1.pending` fallback names; exhaustion leaves the persistent
`overflow.json` sentinel. The helper anchors the directory, adopts only the exact
marker/inode, and enriches it with bounded content-free identity plus a fresh random
128-bit per-adoption commit ID before attestation, ancestor resolution, parsing, and
folding. A missing or rejected token also attempts an independent `hook-<id>.json`
marker. A successful fold records both the marker basename and commit ID in state before
removing that same marker, closing the fold/delete crash window. Valid commit proofs
survive clean startup, resume, and clear boundaries; a reused fixed fallback basename
therefore cannot impersonate another invocation. Legacy basename-only proofs fail
closed. A timeout, crash,
malformed input, or helper that never starts therefore leaves evidence that poisons a
live affected generation to `Unknown` instead of preserving stale work. Once its exact
process incarnation is confirmed gone, the collector omits that unproven generation as
described above. Fault artifacts are bounded. Before generation state can be deleted,
the writer persists its first GC-side
confirmation that the exact process incarnation is gone. That maintenance observation
does not authorize `Done`; it preserves an observation opportunity for the collector.
Deletion requires a later writer pass strictly more than 30 seconds afterward plus a
fresh exact-incarnation gone check. Capacity pressure starts this sequence only after a
terminal generation is at least 30 seconds old or a crashed nonterminal generation is
strictly older than 24 hours; normal cleanup runs on a later `SessionEnd` and requires
the strict 24-hour gate for either. Once the collector has independently observed the
required supported live-to-gone transition, its bounded, content-free,
non-actionable in-memory tombstone preserves the 30-second `Done` row even if generation
state disappears. After draining its payload, every later ingest may also reclaim stale
state/fault temporary files, malformed or abandoned fixed-slot markers, and ordinary
fault markers only when they are strictly older than 24 hours, the complete validated
state snapshot is unchanged across out-of-lock process probes, and every affected
process incarnation is confirmed gone. Collector reads never delete artifacts,
`overflow.json` remains permanent and monotonic, and a clean `SessionStart` does not
delete failure evidence.

JSON and detail views make the distinction auditable with content-free reasons such as
`HookToolOpen`, `HookSubagentActive`, `HookTurnOpen`, `HookTurnComplete`,
`HookInteractionResolutionUnavailable`, `HookEventGap`, `HookConfigChanged`,
`HookStateMalformed`, `HookIntegrationUnverified`, `HerdrScreenBlocked`,
`HerdrScreenWorking`, `HerdrScreenIdle`, and `HerdrWorkingUnrefined`. Hook and Herdr
evidence always use `connection_generation = 0`.

The helper binds each hook event to the nearest eligible native Codex ancestor and its
exact process start identity. Shared daemon hooks, app-server/MCP/Desktop/remote-control
hosts, PID ambiguity, and session/action ownership ambiguity fail closed. An unknown or
inactionable row cannot be killed or terminal-jumped. Codex without the abtop plugin
remains discoverable for rollout metrics; an exactly correlated Herdr terminal can
still provide `Waiting`, `Idle`, or generic `Working`; only `Thinking` and `Executing`
require exact current-helper lifecycle refinement. When plain Codex uses a local or
remote shared app-server daemon, its hooks
run below that shared host instead of the client TUI, so uncorrelated status remains
`Unknown`.

Windows uses that unmanaged behavior too: rollout and process metadata remain available,
but secure native-hook state is currently macOS/Linux-only and lifecycle evidence stays
`Unknown / Unavailable`.

### OpenCode

OpenCode reads the local SQLite database at
`${XDG_DATA_HOME:-~/.local/share}/opencode/opencode.db`; on Windows it also probes
`%LOCALAPPDATA%\opencode` and `%APPDATA%\opencode`. It requires a `sqlite3` CLI on
`PATH` that supports both `-readonly` and `-json`. Discovery considers the 20 most
recently updated database sessions.
An explicit `--session`/`-s` process argument or a unique one-process/one-row cwd group
can confirm lifecycle ownership; ambiguous same-cwd groups become `Unknown`. Process
kill and terminal jump remain disabled because OpenCode has no durable actionable
PID/session registry.

Persisted pending/running `question` and tool rows are not authoritative current
lifecycle: SQLite may expose them before execution or retain them after memory state has
moved on. abtop therefore never promotes those rows to `Wait` or `Exec`. A fresh
incomplete assistant record can provide heuristic `Think`, a completed assistant record
heuristic `Idle`, and a current failure heuristic `Error`. Live permission waits exist
only in process memory and are not observable from the database. The displayed current
task is therefore a generic status label rather than a persisted tool name. Context
percentage uses an estimated 200,000-token window, or 1,000,000 when the model name
contains `[1m]`.

### Grok

Grok reads `active_sessions.json` and per-session `summary.json`, `signals.json`,
`updates.jsonl`, optional `events.jsonl`, and `plan_mode.json` under
`${GROK_HOME:-~/.grok}`. Permission requests, structured questions, and plan approvals
become `Wait` when their unresolved local lifecycle records are present. Grok can also
emit lifecycle `RateLimited`, even though it exposes no account-level quota gauge.

One Grok process can own multiple registered sessions. abtop shows every logical
session, attributes memory, children, and ports only once, and warns that killing the
shared PID stops all of them. Positive lifecycle signals remain usable, but a quiescent
row cannot be proven individually idle while the PID is shared, so provider `Idle` is
downgraded to `Unknown / OwnershipUnconfirmed`. Headless sessions must be registered,
for example with `GROK_TRACK_HEADLESS`; abtop does not guess them from cwd alone.

### Kimi Code

Kimi support targets the current [Kimi Code](https://github.com/MoonshotAI/kimi-code)
CLI and its persisted wire protocol 1.4. It reads `session_index.jsonl`, per-session
`state.json`, and agent `wire.jsonl` under `${KIMI_CODE_HOME:-~/.kimi-code}`. Unsupported,
malformed, incomplete, or stale wire state fails closed to `Unknown`. Retired
`MoonshotAI/kimi-cli` data under `~/.kimi` is not scanned.

Every unresolved `AskUserQuestion`, whether foreground or background, and every
validated running `question` task, including detached tasks, becomes `Wait` until an
exact resolution, cancellation, tool completion, or terminal task snapshot clears it.
Wire protocol 1.4 does not persist ordinary tool-approval prompts, so those remain
`Exec` rather than being guessed from elapsed time or inactivity. A long-running tool
without an explicit interaction record likewise remains `Exec`.

Kimi has no authoritative PID/session registry and can rewrite its process title to
bare `kimi-code`. An explicit visible session association can provide `Provider`
ownership; cwd plus post-start activity can provide useful heuristic display status but
never authorizes kill or jump. Idle old resumes, ambiguous same-root/cwd groups, and
bare-title host ambiguity remain `Unknown` and non-actionable. Visible plugin-runner,
ACP, web, and server host modes are excluded instead of shown as sessions.

For custom Grok or Kimi homes, launch abtop with the same `GROK_HOME` or
`KIMI_CODE_HOME` environment as the agent. abtop also attempts a platform-specific read
of candidate process environments, but operating-system permissions can prevent that
fallback. Grok and Kimi context percentages appear only when their signals or model
configuration provide a reliable window. Their account quota is unavailable from the
native session collectors, but can appear when each provider is enabled and succeeds
in the optional CodexBar integration.

## Themes

12 built-in themes, including 4 colorblind-friendly options (`high-contrast`, `protanopia`, `deuteranopia`, `tritanopia`). Press `t` to cycle and persist a theme, or use `--theme <name>` for a launch-only override. The config overlay can also change and persist the theme.

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

abtop uses the platform config directory returned by the operating system:

| Platform | Config file |
| -------- | ----------- |
| Linux | `${XDG_CONFIG_HOME:-~/.config}/abtop/config.toml` |
| macOS | `~/Library/Application Support/abtop/config.toml` |
| Windows | `%APPDATA%\abtop\config.toml` |

Configuration is loaded at launch. Theme, panel-visibility, and CodexBar quota
changes made in the TUI are written back immediately; unrelated and unknown lines are preserved. The `M`
MCP-session suppression toggle is runtime-only and is not stored.

Supported keys are:

```toml
theme = "btop"

# Optional quotas for all providers enabled in CodexBar.
# The legacy key name is retained for configuration compatibility.
codexbar_quota_fallback = false

# Hide specific agent CLIs from the TUI (case-insensitive).
# Supported IDs: claude, codex, opencode, grok, kimi.
hidden_agents = []

# Additional Claude Code profile roots to scan.
# abtop also auto-discovers ~/.claude and ~/.claude-* roots that contain
# both sessions/ and projects/.
claude_config_dirs = []

# Panel visibility. Every key defaults to true.
show_context = true
show_quota = true
show_tokens = true
show_projects = true
show_ports = true
show_sessions = true
show_mcp = true
```

For example, use `hidden_agents = ["codex", "grok"]` to hide those session collectors
or `claude_config_dirs = ["~/.claude-personal", "~/.claude-work-team"]` to add profile
roots. Account-level quota collection is independent of session visibility, so an
explicitly enabled CodexBar integration remains active when any corresponding session
provider is hidden.

Codex hook integration is intentionally separate from this platform config. It lives
under the active `CODEX_HOME` and is managed with `--setup-codex`,
`--uninstall-codex`, and `--codex-integration-status`.

## Key Bindings

| Key | Action |
| --- | ------ |
| `↑`/`↓`, `k`/`j` | Select the previous or next visible session. |
| `Enter` | Jump by exact Herdr session identity, or by actionable PID after fresh validation. |
| `/` | Enter session-filter input mode. Type to filter, use `Backspace` to delete, `Enter` to keep the filter and leave input mode, or `Esc` to clear it. |
| `x` | Request a kill confirmation; press `x` again within two seconds to kill the same freshly validated session/process incarnation. |
| `X` | Freshly rescan and validate, then kill all processes still owning displayed orphan ports. |
| `r` | Force refresh. Disabled in demo mode. |
| `q` | Quit, or close the config overlay while it is open. |
| `t` | Cycle and persist the theme. |
| `T` | Toggle the subagent tree view. |
| `l` / `L` | Toggle the selected-session timeline. |
| `f` / `F` | Toggle the selected-session file audit. |
| `1`–`7` | Toggle and persist Context, Quota, Tokens, Projects, Ports, Sessions, or MCP visibility. |
| `M` | Toggle suppression of `mcp-server`-owned rollouts in the Sessions panel for this run. |
| `c` | Open/close configuration. Inside it, select with `↑`/`↓` or `k`/`j`, change with `Enter`/Space, and close with `Esc`, `q`, or `c`. |
| `v` | Open/close the view menu. |
| `Esc` | Close the view menu, or clear a retained nonempty session filter outside filter-input mode. |
| `?` | Show keybinding help; any key closes it. |

In the narrow tabbed layout:

| Key | Action |
| --- | ------ |
| `←`/`→`, `Shift+Tab`/`Tab` | Cycle visible Work, Usage, and System tabs. |
| `w`, `u`, `s` | Select the Work, Usage, or System tab directly. |
| `+` / `=` | Maximize the active section. |
| `-` | Restore the split sections. |

In demo mode, the keyboard actions `r`, `x`, `X`, and `Enter` are disabled. Do not treat
`--demo --mouse` as a destructive-action safety boundary: the mouse orphan-port cleanup
target is still active.

## Library / JSON snapshot

abtop is also a library crate, so local tools can reuse its collection and
state APIs in-process and serialize the same state the TUI renders.

```bash
abtop --json    # one-shot JSON snapshot for scripts
```

For long-running consumers, build an `App`, refresh it with
`App::tick_no_summaries()` (which never spawns `claude --print`, so it doesn't
touch your Claude quota), and call `App::to_snapshot(interval_ms)` to get a
JSON-serializable `Snapshot`:

```rust,no_run
use abtop::app::App;
use abtop::{config, theme::Theme};

let cfg = config::load_config();
let mut app = App::new_with_config_and_claude_dirs_and_codexbar(
    Theme::default(), &cfg.hidden_agents, cfg.panels, &cfg.claude_config_dirs,
    cfg.codexbar_quota_fallback,
);
app.tick_no_summaries();
let json = serde_json::to_string(&app.to_snapshot(2_000)).unwrap();
```

`App` is not `Send` (it owns the collectors), so keep it on one thread and pass
the serialized JSON elsewhere. [abtop-web-ui](https://github.com/XKHoshizora/abtop-web-ui)
is a reference consumer: a local-first web dashboard built on exactly this API.

## Privacy

abtop collectors read local files and local process/open-file metadata, including the
Claude, Codex, OpenCode, Grok, and Kimi Code session stores. They need no provider API
keys and do not send collected session records to provider APIs. Normal collection
starts no Codex relay or daemon, attaches to no Codex daemon, and uses no provider API
or transport credential. The disabled-by-default CodexBar integration is the explicit
exception: its short-lived, bounded command uses CodexBar's enabled providers and
automatic sources, which may access existing local, authenticated web, OAuth, or API
sessions. abtop never reads or stores those credentials. It retains only bounded quota
windows, provenance, freshness, and fixed sanitized diagnostics—never account identity,
email, organization, credits, pace summaries, dashboard data, raw errors, or arbitrary
provider text.

Native Codex can independently choose a shared local or remote app-server daemon; those
sessions deliberately remain `Unknown`. Codex otherwise continues to run directly with
the caller's executable, arguments, standard streams, and environment; abtop does not
inspect or persist provider credentials.

The Codex hook helper uses a 4 MiB streaming JSON parser and never materializes the raw
payload as one buffer. It accepts at most 256 root fields, 512 bytes per lifecycle ID,
16 KiB of cwd, and the other small allowlisted lifecycle scalars; every unrecognized or
sensitive value is skipped directly by the deserializer. It attempts to drain to EOF even
after malformed JSON, subject to the hard stream cap. Prompt text, tool input and output,
the last assistant message, raw commands and arguments, environment and authentication
data, transcript paths, and arbitrary provider text never enter state. Private state
contains only schema/helper/install identities, session/turn/tool/subagent identifiers,
canonical event and tool classes, cwd when needed for correlation, timestamps, exact
PID/start incarnations, lifecycle faults/open sets, and at most 128 content-free samples.

Codex plugin data directories use mode `0700` and files use mode `0600` on Unix. State
writes reject symlinks and ownership mismatches, lock updates, and atomically replace a
same-directory file. Malformed input, event gaps, changed hook/helper identities,
unsafe paths, stale state, or ambiguous native-process ancestry become sticky failure
evidence and keep an affected live generation `Unknown` unless exact Herdr evidence
provides one of its independent states. The launch marker is created before the helper starts, adopted and enriched
before validation, and removed only after its basename and fresh random 128-bit
per-adoption commit ID are durably committed by a successful fold. The POSIX launcher
normally allocates a unique `launch-<shell-pid>-pending.<16-alphanumeric-nonce>` marker;
16 no-clobber fixed names are a bounded fallback only. Valid commit proofs survive clean
generation boundaries, so even reuse of a fallback basename cannot hide a different
failed invocation. A helper timeout, crash, rejected record, or failed launch therefore
cannot leave stale positive evidence. Missing-token fallback markers, bounded ordinary
faults, and persistent overflow prevent unbounded artifacts while preserving fail-closed
evidence. Before deleting generation state, a
writer persists its first GC-side exact-incarnation gone confirmation; that timestamp is
only a deletion grace anchor and never authorizes `Done`. Removal requires a later pass
strictly more than 30 seconds afterward and a fresh exact-incarnation gone check.
Capacity pressure starts that sequence only after a terminal generation is at least 30
seconds old or a crashed nonterminal generation is strictly older than 24 hours; normal
cleanup runs on a later `SessionEnd` and requires the strict 24-hour gate for either.
The collector separately requires an already observed exact supported live-to-gone
transition and retains a bounded, content-free, non-actionable 30-second in-memory
tombstone, so proven `Done` survives later source-state disappearance. A collector whose
first observation is already gone, numeric-PID reuse without exact incarnation
continuity, or an unavailable scan cannot create that proof. Once the exact incarnation
is confirmed gone, an unproven hook-only generation is omitted instead of displayed as
PID-zero `Unknown`. Once its payload is drained,
each later ingest can reclaim strictly
older-than-24-hour temporary files, malformed or abandoned fixed-slot markers, and
ordinary faults only from a complete validated state snapshot that remains unchanged
across out-of-lock process-death probes, with every affected incarnation confirmed
gone. Collector reads never remove artifacts and `overflow.json` remains permanent and
monotonic. These guarantees are a content boundary, not a claim that lifecycle metadata
such as cwd and stable IDs is nonsensitive.

`abtop --setup-codex` can create the absolute `CODEX_HOME` when it is absent, then writes
the isolated marketplace/plugin source bundle, the retained private
`$CODEX_HOME/.abtop-codex-plugin.lock`, the content-free `installation.json`
attestation and `states/faults` tree under plugin data, Codex's installed plugin cache and
native marketplace/plugin registration, and the content-free stable migration lock
described above on macOS/Linux. Generated launchers contain the exact absolute abtop
executable and private plugin-data paths, but no provider content. It never
modifies global `hooks.json`,
`notify`, OpenTelemetry, unrelated plugins, Claude configuration, `PATH`, or the Codex
executable. During migration it may remove exact legacy abtop wrapper marker blocks
from shell startup files; it does not remove arbitrary aliases or functions. Uninstall
uses the same exact-marker rule, unconditionally removes and then verifies absence of the
reserved plugin ID, verifies marketplace absence before deleting the owned source tree,
and preserves both the retained root setup lock and the content-free plugin-data tree and
attestation. It removes a legacy source-local `abtop/.setup.lock` only as part of verified
owned-source cleanup.
The helper identity in the declared hook command deliberately retriggers Codex trust
review after meaningful updates.

Codex hook setup and secure state collection are currently available only on macOS and
Linux. Windows setup fails before mutation; ordinary Windows Codex collection remains
read-only and reports lifecycle status as `Unknown`.

`abtop --setup` writes `abtop-statusline.sh` inside the active Claude config root and
registers the script in that root's `settings.json`. When StatusLine input contains
`rate_limits`, the hook extracts only quota percentages and reset timestamps through
local `python3`, adds the fixed source `claude` and current local update time, and writes
`abtop-rate-limits.json`. It persists no prompt or message content. abtop does not add a
network request to that hook.

abtop uses the operating system's cache directory:

| Platform | abtop cache directory |
| -------- | --------------------- |
| Linux | `${XDG_CACHE_HOME:-~/.cache}/abtop/` |
| macOS | `~/Library/Caches/abtop/` |
| Windows | `%LOCALAPPDATA%\abtop\` |

The directory can contain `summaries.json` and `codex-rate-limits.json`. The latter
stores only the last locally observed native Codex account quota windows and timestamps.
CodexBar values for every provider remain in memory and are never written to this cache.
Codex hook lifecycle state is kept separately at
`${CODEX_HOME:-~/.codex}/plugins/data/abtop-abtop-local`; it contains only the bounded
content-free fields described above.

The normal TUI and `--once` generate missing session titles by passing up to 200
characters from the first user text and up to 200 characters from the first assistant
text to the locally installed `claude --print`. That CLI may call Anthropic. The summary
cache's `summaries.json` stores only the derived title or sanitized 80-character fallback
for each session, not the full source records; those cached values can still contain
sensitive project context. `App::tick_no_summaries()` does not launch summary jobs or
pass excerpts to `claude --print`; neither does `abtop --json`.

The TUI and `--once` do not deliberately render full local files or raw tool results,
but they do show safe path previews, bounded tool-argument and child-command previews,
and bounded session summaries. The selected-session detail can also render the
collected `initial_prompt` field and a bounded, redacted tail of recent user and
assistant chat messages. Prompts or chat can themselves contain pasted file content.
The JSON snapshot includes richer local dashboard data such as `summary`,
bounded/redacted `chat_messages`, working directories, config roots, tool-call previews,
child process commands, token counts, and port metadata. All of these outputs and caches
can reveal project context; treat them as private data and do not expose them through
shared logs or a network without your own access controls.

## Acknowledgements

Huge thanks to [@tbouquet](https://github.com/tbouquet) for driving much of abtop's recent shape — themes, config overlay and panel toggles, session filtering, subagent tree view, the context window gauge with compaction detection, plus a steady stream of fixes and security hardening along the way.

## License

MIT
