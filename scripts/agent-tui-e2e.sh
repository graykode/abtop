#!/usr/bin/env bash
# Manual, authenticated end-to-end checks for abtop's terminal integrations.
#
# The harness deliberately uses a private agent-tui daemon and an exact named
# Herdr session. It never issues a global agent-tui cleanup and never attaches
# to, focuses, stops, or deletes Herdr's default session.

set -Eeuo pipefail
umask 077

readonly SCRIPT_NAME="${0##*/}"
readonly CONFIG_LABEL="CodexBar quotas"
readonly CODEXBAR_IDENTITY_SENTINEL="ABTOP_E2E_PRIVATE_IDENTITY_DETAIL"
readonly CODEXBAR_CREDITS_SENTINEL="ABTOP_E2E_PRIVATE_CREDITS_DETAIL"
readonly CODEXBAR_PACE_SENTINEL="ABTOP_E2E_PRIVATE_PACE_DETAIL"
readonly CODEXBAR_ERROR_SENTINEL="ABTOP_E2E_PRIVATE_RAW_ERROR_DETAIL"

SUITE=""
ABTOP_BIN=""
CODEX_HOME_PATH=""
ARTIFACTS_PARENT=""
WORKSPACE_PATH=""
AGENT_TUI_BIN=""
RUN_ROOT=""
RUNTIME_ROOT=""
DAEMON_STARTED=0
HERDR_MAY_EXIST=0
HERDR_SOCKET=""
HERDR_NAME=""
HERDR_TUI_SESSION=""
ACTIVE_ATUI_SESSIONS=()
LAST_ATUI_SESSION=""

usage() {
    cat <<EOF
Usage:
  $SCRIPT_NAME --suite codex-status|codexbar|all \\
    --abtop /absolute/path/to/abtop \\
    [--codex-home /absolute/path/to/CODEX_HOME] \\
    [--workspace /absolute/path/to/trusted/workspace] \\
    [--artifacts /path/to/artifact-parent]

Suites:
  codexbar     Deterministic fake-CodexBar toggle, persistence, partial-result,
               privacy, JSON, and provider quota-rendering checks. Does not use
               Codex authentication.
  codex-status Authenticated Codex lifecycle and exact Herdr focus checks in a
               disposable named Herdr session. Requires an already healthy and
               trusted abtop Codex integration, --codex-home, and consumes four
               small Codex turns across two disposable Codex sessions.
  all          Run codexbar first, then codex-status; requires --codex-home.

The artifact directory is retained and may contain controlled terminal screen
captures. The harness never installs dependencies or changes Codex hook trust.
EOF
}

log() {
    printf '[agent-tui-e2e] %s\n' "$*"
}

fail() {
    printf '[agent-tui-e2e] ERROR: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

version_at_least() {
    local actual="$1"
    local required="$2"
    local actual_major actual_minor actual_patch required_major required_minor required_patch
    IFS=. read -r actual_major actual_minor actual_patch <<<"$actual"
    IFS=. read -r required_major required_minor required_patch <<<"$required"
    actual_patch="${actual_patch%%[^0-9]*}"
    required_patch="${required_patch%%[^0-9]*}"
    (( actual_major > required_major )) && return 0
    (( actual_major < required_major )) && return 1
    (( actual_minor > required_minor )) && return 0
    (( actual_minor < required_minor )) && return 1
    (( actual_patch >= required_patch ))
}

parse_args() {
    while (($# > 0)); do
        case "$1" in
            --suite)
                (($# >= 2)) || fail "--suite requires a value"
                SUITE="$2"
                shift 2
                ;;
            --abtop)
                (($# >= 2)) || fail "--abtop requires a value"
                ABTOP_BIN="$2"
                shift 2
                ;;
            --codex-home)
                (($# >= 2)) || fail "--codex-home requires a value"
                CODEX_HOME_PATH="$2"
                shift 2
                ;;
            --artifacts)
                (($# >= 2)) || fail "--artifacts requires a value"
                ARTIFACTS_PARENT="$2"
                shift 2
                ;;
            --workspace)
                (($# >= 2)) || fail "--workspace requires a value"
                WORKSPACE_PATH="$2"
                shift 2
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                fail "unknown argument: $1"
                ;;
        esac
    done

    case "$SUITE" in
        codex-status|codexbar|all) ;;
        *) fail "--suite must be codex-status, codexbar, or all" ;;
    esac
    [[ "$ABTOP_BIN" == /* ]] || fail "--abtop must be an absolute path"
    [[ -f "$ABTOP_BIN" && -x "$ABTOP_BIN" ]] || fail "--abtop is not an executable file: $ABTOP_BIN"
    if [[ "$SUITE" == "codex-status" || "$SUITE" == "all" ]]; then
        [[ "$CODEX_HOME_PATH" == /* ]] || fail "--codex-home must be an absolute path"
        [[ -d "$CODEX_HOME_PATH" ]] || fail "--codex-home is not a directory: $CODEX_HOME_PATH"
    fi
    if [[ -z "$WORKSPACE_PATH" ]]; then
        WORKSPACE_PATH="$(cd "$(dirname "$0")/.." && pwd -P)"
    fi
    [[ "$WORKSPACE_PATH" == /* ]] || fail "--workspace must be an absolute path"
    [[ -d "$WORKSPACE_PATH" ]] || fail "--workspace is not a directory: $WORKSPACE_PATH"
}

register_atui_session() {
    ACTIVE_ATUI_SESSIONS+=("$1")
}

atui() {
    local session_id="$1"
    shift
    "$AGENT_TUI_BIN" --session "$session_id" --json "$@"
}

kill_atui_session() {
    local session_id="$1"
    set +e
    atui "$session_id" kill --yes >/dev/null 2>&1
    set -e
}

cleanup() {
    local status=$?
    local session_id
    local runtime_entry
    trap - EXIT HUP INT TERM
    set +e

    for session_id in "${ACTIVE_ATUI_SESSIONS[@]}"; do
        atui "$session_id" kill --yes >/dev/null 2>&1
    done

    if ((HERDR_MAY_EXIST)); then
        herdr session stop "$HERDR_NAME" --json >/dev/null 2>&1
        herdr session delete "$HERDR_NAME" --json >/dev/null 2>&1
    fi

    if ((DAEMON_STARTED)); then
        "$AGENT_TUI_BIN" --json daemon stop --yes >/dev/null 2>&1
    fi

    if [[ -n "$RUN_ROOT" ]]; then
        if [[ -n "$RUNTIME_ROOT" && -f "$RUNTIME_ROOT/agent-tui.log" ]]; then
            cp "$RUNTIME_ROOT/agent-tui.log" "$RUN_ROOT/agent-tui.log"
        fi
        if [[ -n "$RUNTIME_ROOT" && -f "$RUNTIME_ROOT/sessions.jsonl" ]]; then
            cp "$RUNTIME_ROOT/sessions.jsonl" "$RUN_ROOT/agent-tui-sessions.jsonl"
        fi
        if [[ -n "$RUNTIME_ROOT" && -f "$RUNTIME_ROOT/a.log" ]]; then
            cp "$RUNTIME_ROOT/a.log" "$RUN_ROOT/agent-tui-daemon.log"
        fi
        printf '%s\n' "$status" >"$RUN_ROOT/exit-code"
        log "artifacts retained at $RUN_ROOT"
    fi

    case "$RUNTIME_ROOT" in
        /tmp/abtop-atui.*|/private/tmp/abtop-atui.*)
            for runtime_entry in \
                a.sock a.lock a.log sessions.jsonl sessions.lock \
                agent-tui.log ui.json ws.json; do
                if [[ -e "$RUNTIME_ROOT/$runtime_entry" || -L "$RUNTIME_ROOT/$runtime_entry" ]]; then
                    rm -f -- "$RUNTIME_ROOT/$runtime_entry"
                fi
            done
            rmdir "$RUNTIME_ROOT" >/dev/null 2>&1 || true
            ;;
    esac
    exit "$status"
}

prepare_runtime() {
    local run_id version_output version
    require_command jq
    require_command mktemp
    require_command agent-tui
    AGENT_TUI_BIN="$(command -v agent-tui)"

    version_output="$($AGENT_TUI_BIN --version 2>&1)"
    version="$(printf '%s\n' "$version_output" | sed -E -n 's/.*[^0-9]([0-9]+\.[0-9]+\.[0-9]+).*/\1/p' | head -n 1)"
    [[ -n "$version" ]] || fail "could not parse agent-tui version from: $version_output"
    version_at_least "$version" "1.1.0" || fail "agent-tui 1.1.0 or newer is required (found $version)"

    run_id="abtop-e2e-$(date +%s)-$$"
    if [[ -n "$ARTIFACTS_PARENT" ]]; then
        mkdir -p "$ARTIFACTS_PARENT"
        ARTIFACTS_PARENT="$(cd "$ARTIFACTS_PARENT" && pwd -P)"
        RUN_ROOT="$ARTIFACTS_PARENT/$run_id"
        mkdir "$RUN_ROOT"
    else
        RUN_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/abtop-agent-tui-artifacts.XXXXXX")"
    fi

    # Keep the Unix socket path short enough for macOS. All agent-tui state is
    # still private and exact; only final evidence is copied to RUN_ROOT.
    RUNTIME_ROOT="$(mktemp -d "/tmp/abtop-atui.XXXXXX")"
    export AGENT_TUI_SOCKET="$RUNTIME_ROOT/a.sock"
    export AGENT_TUI_SESSION_STORE="$RUNTIME_ROOT/sessions.jsonl"
    export AGENT_TUI_UI_STATE="$RUNTIME_ROOT/ui.json"
    export AGENT_TUI_WS_STATE="$RUNTIME_ROOT/ws.json"
    export AGENT_TUI_LOG="$RUNTIME_ROOT/agent-tui.log"
    export AGENT_TUI_LOG_FORMAT=json
    export AGENT_TUI_LOG_STREAM=stderr
    export AGENT_TUI_TRANSPORT=unix
    export AGENT_TUI_WS_DISABLED=1
    export AGENT_TUI_NO_INPUT=1
    export NO_COLOR=1

    printf '%s\n' "$version_output" >"$RUN_ROOT/agent-tui-version.txt"
    "$ABTOP_BIN" --version >"$RUN_ROOT/abtop-version.txt"
    "$AGENT_TUI_BIN" --json daemon start >"$RUN_ROOT/agent-tui-daemon-start.json"
    DAEMON_STARTED=1
    "$AGENT_TUI_BIN" --json daemon status >"$RUN_ROOT/agent-tui-daemon-status.json"
}

run_atui_session() {
    local output_file="$1"
    shift
    LAST_ATUI_SESSION=""
    "$AGENT_TUI_BIN" --json run "$@" >"$output_file"
    [[ -s "$output_file" ]] || fail "agent-tui returned empty run output"
    LAST_ATUI_SESSION="$(jq -er '.session_id | select(type == "string" and length > 0)' "$output_file")"
    register_atui_session "$LAST_ATUI_SESSION"
}

capture_screen() {
    local session_id="$1"
    local basename="$2"
    atui "$session_id" screenshot --strip-ansi >"$RUN_ROOT/$basename.json"
    jq -er '.screenshot' "$RUN_ROOT/$basename.json" >"$RUN_ROOT/$basename.txt"
}

wait_screen_regex() {
    local session_id="$1"
    local regex="$2"
    local timeout_seconds="$3"
    local evidence_name="$4"
    local deadline now poll poll_name
    deadline=$(( $(date +%s) + timeout_seconds ))
    poll=0
    while :; do
        poll=$((poll + 1))
        printf -v poll_name '%s-poll-%03d' "$evidence_name" "$poll"
        capture_screen "$session_id" "$poll_name"
        if grep -E -q "$regex" "$RUN_ROOT/$poll_name.txt"; then
            cp "$RUN_ROOT/$poll_name.json" "$RUN_ROOT/$evidence_name.json"
            cp "$RUN_ROOT/$poll_name.txt" "$RUN_ROOT/$evidence_name.txt"
            return 0
        fi
        now=$(date +%s)
        ((now < deadline)) || return 1
        sleep 1
    done
}

wait_codex_table_row_count() {
    local session_id="$1"
    local expected_count="$2"
    local timeout_seconds="$3"
    local evidence_name="$4"
    local deadline now poll poll_name actual_count consecutive_matches required_matches
    deadline=$(( $(date +%s) + timeout_seconds ))
    poll=0
    consecutive_matches=0
    required_matches=1
    if [[ "$expected_count" == 0 ]]; then
        required_matches=2
    fi
    while :; do
        poll=$((poll + 1))
        printf -v poll_name '%s-poll-%03d' "$evidence_name" "$poll"
        if capture_screen "$session_id" "$poll_name" \
            && [[ -s "$RUN_ROOT/$poll_name.txt" ]] \
            && grep -E -q 'abtop v[0-9]' "$RUN_ROOT/$poll_name.txt" \
            && grep -F -q '⁶sessions' "$RUN_ROOT/$poll_name.txt" \
            && grep -E -q \
                'AI[[:space:]]+Pid[[:space:]]+Project' \
                "$RUN_ROOT/$poll_name.txt"; then
            actual_count="$(grep -E -c \
                '[[:space:]]>?CD[[:space:]]+[0-9]+[[:space:]]' \
                "$RUN_ROOT/$poll_name.txt" || true)"
            if [[ "$actual_count" == "$expected_count" ]]; then
                consecutive_matches=$((consecutive_matches + 1))
                if ((consecutive_matches >= required_matches)); then
                    cp "$RUN_ROOT/$poll_name.json" "$RUN_ROOT/$evidence_name.json"
                    cp "$RUN_ROOT/$poll_name.txt" "$RUN_ROOT/$evidence_name.txt"
                    return 0
                fi
            else
                consecutive_matches=0
            fi
        else
            consecutive_matches=0
        fi
        now=$(date +%s)
        ((now < deadline)) || return 1
        sleep 1
    done
}

assert_screen_regex() {
    local basename="$1"
    local regex="$2"
    grep -E -q "$regex" "$RUN_ROOT/$basename.txt" \
        || fail "screen $basename did not match: $regex"
}

assert_screen_absent() {
    local basename="$1"
    local literal="$2"
    if grep -F -q "$literal" "$RUN_ROOT/$basename.txt"; then
        fail "screen $basename exposed or retained forbidden text: $literal"
    fi
}

assert_codexbar_private_data_absent() {
    local path="$1"
    local sentinel
    for sentinel in \
        "$CODEXBAR_IDENTITY_SENTINEL" \
        "$CODEXBAR_CREDITS_SENTINEL" \
        "$CODEXBAR_PACE_SENTINEL" \
        "$CODEXBAR_ERROR_SENTINEL"; do
        if grep -F -q "$sentinel" "$path"; then
            fail "CodexBar output exposed private provider data in $path"
        fi
    done
}

open_codexbar_config() {
    local session_id="$1"
    local evidence_prefix="$2"
    atui "$session_id" press c >/dev/null
    wait_screen_regex "$session_id" "$CONFIG_LABEL" 10 "$evidence_prefix-open" \
        || fail "CodexBar config row did not appear"
    atui "$session_id" press \
        ArrowDown ArrowDown ArrowDown ArrowDown ArrowDown ArrowDown ArrowDown ArrowDown >/dev/null
    capture_screen "$session_id" "$evidence_prefix-selected"
    assert_screen_regex "$evidence_prefix-selected" ">[[:space:]]+$CONFIG_LABEL"
}

write_fake_codexbar() {
    local fake_bin="$1"
    mkdir -p "$fake_bin"
    cat >"$fake_bin/codexbar" <<'FAKE_CODEXBAR'
#!/bin/sh
set -eu
: "${ABTOP_E2E_CODEXBAR_MODE_FILE:?}"
: "${ABTOP_E2E_CODEXBAR_CALLS_FILE:?}"
printf '%s\n' "$*" >>"$ABTOP_E2E_CODEXBAR_CALLS_FILE"
if [ "$*" != "usage --format json --json-only --no-color" ]; then
    printf '%s\n' "unexpected CodexBar arguments" >&2
    exit 64
fi
mode=$(sed -n '1p' "$ABTOP_E2E_CODEXBAR_MODE_FILE")
case "$mode" in
    partial)
        # Cross an epoch-second boundary so headless paths prove they take
        # their freshness clock after the blocking initial poll completes.
        sleep 2
        printf '%s\n' '[{"provider":"claude","source":"claude","usage":{"primary":{"usedPercent":28.0,"windowMinutes":300,"resetsAt":"2099-01-01T05:00:00Z"},"secondary":{"usedPercent":6.0,"windowMinutes":10080,"resetsAt":"2099-01-08T00:00:00Z"},"tertiary":{"usedPercent":11.0,"windowMinutes":43200,"resetsAt":"2099-01-31T00:00:00Z"},"extraRateWindows":[{"id":"claude-weekly-scoped-fable","title":"Fable only","window":{"usedPercent":0.0,"windowMinutes":10080,"resetsAt":"2099-01-08T08:00:00Z"}}],"identity":{"providerID":"claude","accountEmail":"ABTOP_E2E_PRIVATE_IDENTITY_DETAIL"}},"pace":{"primary":{"summary":"ABTOP_E2E_PRIVATE_PACE_DETAIL"}}},{"provider":"codex","source":"oauth","usage":{"primary":null,"secondary":{"usedPercent":48.0,"windowMinutes":10080,"resetsAt":"2099-01-08T09:00:00Z"},"tertiary":null,"extraRateWindows":[{"id":"codex-spark-weekly","title":"Codex Spark Weekly","window":{"usedPercent":0.0,"windowMinutes":10080,"resetsAt":"2099-01-09T00:00:00Z"}}],"identity":{"providerID":"codex"}},"credits":{"remaining":0,"events":[{"detail":"ABTOP_E2E_PRIVATE_CREDITS_DETAIL"}]}},{"provider":"kimi","source":"web","error":{"kind":"provider","code":1,"message":"ABTOP_E2E_PRIVATE_RAW_ERROR_DETAIL"}},{"provider":"grok","source":"grok-web","usage":{"primary":{"usedPercent":18.0,"resetsAt":"2099-01-04T06:36:08Z"},"secondary":null,"tertiary":null,"extraRateWindows":[],"identity":{"providerID":"grok"}}}]'
        printf '%s\n' "ABTOP_E2E_PRIVATE_RAW_ERROR_DETAIL" >&2
        exit 42
        ;;
    error)
        printf '%s\n' "ABTOP_E2E_PRIVATE_RAW_ERROR_DETAIL" >&2
        exit 42
        ;;
    *)
        printf '%s\n' "unknown fake CodexBar mode" >&2
        exit 65
        ;;
esac
FAKE_CODEXBAR
    chmod 700 "$fake_bin/codexbar"
}

start_codexbar_abtop() {
    local evidence_name="$1"
    local home="$2"
    local isolated_codex_home="$3"
    local fake_bin="$4"
    local mode_file="$5"
    local calls_file="$6"
    local session_id
    run_atui_session "$RUN_ROOT/$evidence_name-run.json" \
        --cols 160 --rows 48 --cwd "$RUN_ROOT" -- \
        env \
        "HOME=$home" \
        "XDG_CONFIG_HOME=$home/.config" \
        "CODEX_HOME=$isolated_codex_home" \
        "PATH=$fake_bin:$PATH" \
        "ABTOP_E2E_CODEXBAR_MODE_FILE=$mode_file" \
        "ABTOP_E2E_CODEXBAR_CALLS_FILE=$calls_file" \
        "$ABTOP_BIN"
    session_id="$LAST_ATUI_SESSION"
    wait_screen_regex "$session_id" "abtop" 15 "$evidence_name-ready" \
        || fail "abtop did not render in agent-tui session $session_id"
}

find_isolated_config() {
    local home="$1"
    local config_file
    case "$(uname -s)" in
        Darwin)
            config_file="$home/Library/Application Support/abtop/config.toml"
            ;;
        Linux)
            config_file="$home/.config/abtop/config.toml"
            ;;
        *)
            fail "agent-tui E2E tests support only macOS and Linux"
            ;;
    esac
    [[ -f "$config_file" ]] || fail "abtop did not persist an isolated config file"
    printf '%s\n' "$config_file"
}

seed_isolated_codexbar_config() {
    local home="$1"
    local config_dir
    case "$(uname -s)" in
        Darwin)
            config_dir="$home/Library/Application Support/abtop"
            ;;
        Linux)
            config_dir="$home/.config/abtop"
            ;;
        *)
            fail "agent-tui E2E tests support only macOS and Linux"
            ;;
    esac
    mkdir -p "$config_dir"
    cat >"$config_dir/config.toml" <<'ISOLATED_CONFIG'
codexbar_quota_fallback = false
hidden_agents = ["claude", "codex", "opencode", "grok", "kimi"]
show_context = false
show_quota = true
show_tokens = false
show_projects = false
show_ports = false
show_sessions = false
show_mcp = false
ISOLATED_CONFIG
}

run_codexbar_suite() {
    local suite_root home isolated_codex_home fake_bin mode_file calls_file
    local initial_id persisted_id error_id off_id config_file calls_before calls_after
    suite_root="$RUN_ROOT/codexbar"
    home="$suite_root/home"
    isolated_codex_home="$suite_root/empty-codex-home"
    fake_bin="$suite_root/fake-bin"
    mode_file="$suite_root/mode"
    calls_file="$suite_root/calls.log"
    mkdir -p "$home" "$isolated_codex_home"
    seed_isolated_codexbar_config "$home"
    : >"$calls_file"
    printf '%s\n' partial >"$mode_file"
    write_fake_codexbar "$fake_bin"

    log "CodexBar: asserting initial off state and interactive enable"
    start_codexbar_abtop "codexbar-initial" "$home" "$isolated_codex_home" "$fake_bin" "$mode_file" "$calls_file"
    initial_id="$LAST_ATUI_SESSION"
    open_codexbar_config "$initial_id" "codexbar-initial"
    assert_screen_regex "codexbar-initial-selected" "${CONFIG_LABEL}[[:space:]]+off"
    atui "$initial_id" press Enter >/dev/null
    wait_screen_regex "$initial_id" "${CONFIG_LABEL}[[:space:]]+partial([[:space:]]|$)" 20 "codexbar-partial-config" \
        || fail "CodexBar quotas did not reach partial state"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-partial-config.txt"
    atui "$initial_id" press Esc >/dev/null
    wait_screen_regex "$initial_id" "KIMI·CB" 10 "codexbar-partial-quota" \
        || fail "quota panel did not render the unavailable Kimi provider"
    assert_screen_regex "codexbar-partial-quota" "unavailable"
    assert_screen_regex "codexbar-partial-quota" "CLAUDE·CB"
    assert_screen_regex "codexbar-partial-quota" "CODEX·CB"
    assert_screen_regex "codexbar-partial-quota" "GROK·CB"
    assert_screen_regex "codexbar-partial-quota" "Tertiary"
    assert_screen_regex "codexbar-partial-quota" "Fable only"
    assert_screen_regex "codexbar-partial-quota" "Codex Spark Week"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-partial-quota.txt"
    config_file="$(find_isolated_config "$home")"
    grep -E -q '^codexbar_quota_fallback[[:space:]]*=[[:space:]]*true$' "$config_file" \
        || fail "CodexBar enabled setting was not persisted"
    grep -F -q 'usage --format json --json-only --no-color' "$calls_file" \
        || fail "fake CodexBar did not receive the exact bounded command"
    kill_atui_session "$initial_id"

    log "CodexBar: asserting enabled-state persistence and JSON diagnostics"
    start_codexbar_abtop "codexbar-persisted" "$home" "$isolated_codex_home" "$fake_bin" "$mode_file" "$calls_file"
    persisted_id="$LAST_ATUI_SESSION"
    wait_screen_regex "$persisted_id" "KIMI·CB" 20 "codexbar-persisted-quota" \
        || fail "persisted CodexBar setting did not restore provider quotas"
    assert_screen_regex "codexbar-persisted-quota" "unavailable"
    assert_screen_regex "codexbar-persisted-quota" "CLAUDE·CB"
    assert_screen_regex "codexbar-persisted-quota" "CODEX·CB"
    assert_screen_regex "codexbar-persisted-quota" "GROK·CB"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-persisted-quota.txt"
    open_codexbar_config "$persisted_id" "codexbar-persisted"
    wait_screen_regex "$persisted_id" "${CONFIG_LABEL}[[:space:]]+partial([[:space:]]|$)" 10 "codexbar-persisted-config" \
        || fail "persisted config did not show partial CodexBar state"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-persisted-config.txt"
    env \
        "HOME=$home" \
        "XDG_CONFIG_HOME=$home/.config" \
        "CODEX_HOME=$isolated_codex_home" \
        "PATH=$fake_bin:$PATH" \
        "ABTOP_E2E_CODEXBAR_MODE_FILE=$mode_file" \
        "ABTOP_E2E_CODEXBAR_CALLS_FILE=$calls_file" \
        "$ABTOP_BIN" --json \
        >"$RUN_ROOT/codexbar-partial-snapshot.json" \
        2>"$RUN_ROOT/codexbar-partial-snapshot.stderr.txt"
    jq -e '
        .codexbar_quota.enabled == true and
        .codexbar_quota.state == "partial" and
        .codexbar_quota.provenance == "codexbar" and
        .codexbar_quota.stale == false and
        [.codexbar_quota.providers[].provider] == ["claude", "codex", "grok", "kimi"] and
        all(.codexbar_quota.providers[0:3][];
            .state == "active" and
            .provenance == "codexbar" and
            .stale == false and
            .error == null
        ) and
        .codexbar_quota.providers[3].provider == "kimi" and
        .codexbar_quota.providers[3].state == "unavailable" and
        .codexbar_quota.providers[3].provenance == null and
        .codexbar_quota.providers[3].stale == false and
        .codexbar_quota.providers[3].error == "provider_error" and
        any(.rate_limits[];
            .source == "claude" and
            (.windows | length) == 4 and
            any(.windows[];
                .id == "tertiary" and
                .used_pct == 11 and
                .window_minutes == 43200 and
                .provenance == "codexbar"
            ) and
            any(.windows[];
                .id == "claude-weekly-scoped-fable" and
                .label == "Fable only" and
                .used_pct == 0 and
                .window_minutes == 10080 and
                .provenance == "codexbar"
            )
        ) and
        any(.rate_limits[];
            .source == "codex" and
            any(.windows[];
                .id == "codex-spark-weekly" and
                .label == "Codex Spark Weekly" and
                .used_pct == 0 and
                .window_minutes == 10080 and
                .provenance == "codexbar"
            )
        ) and
        any(.rate_limits[];
            .source == "grok" and
            any(.windows[];
                .id == "primary" and
                .used_pct == 18 and
                .resets_at != null and
                .window_minutes == null and
                .provenance == "codexbar"
            )
        ) and
        all(.rate_limits[].windows[];
            (.id | type) == "string" and
            (.label | type) == "string"
        )
    ' "$RUN_ROOT/codexbar-partial-snapshot.json" >/dev/null \
        || fail "partial CodexBar JSON snapshot did not match the provider/window contract"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-partial-snapshot.json"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-partial-snapshot.stderr.txt"

    env \
        "HOME=$home" \
        "XDG_CONFIG_HOME=$home/.config" \
        "CODEX_HOME=$isolated_codex_home" \
        "PATH=$fake_bin:$PATH" \
        "ABTOP_E2E_CODEXBAR_MODE_FILE=$mode_file" \
        "ABTOP_E2E_CODEXBAR_CALLS_FILE=$calls_file" \
        "$ABTOP_BIN" --once \
        >"$RUN_ROOT/codexbar-partial-once.txt" \
        2>"$RUN_ROOT/codexbar-partial-once.stderr.txt"
    grep -E -q '^quota:$' "$RUN_ROOT/codexbar-partial-once.txt" \
        || fail "--once omitted the quota section"
    grep -E -q '^  claude:' "$RUN_ROOT/codexbar-partial-once.txt" \
        || fail "--once omitted Claude CodexBar quota"
    grep -E -q '^  codex:' "$RUN_ROOT/codexbar-partial-once.txt" \
        || fail "--once omitted Codex CodexBar quota"
    grep -E -q '^  grok:' "$RUN_ROOT/codexbar-partial-once.txt" \
        || fail "--once omitted Grok CodexBar quota"
    grep -E -q '^  kimi: unavailable$' "$RUN_ROOT/codexbar-partial-once.txt" \
        || fail "--once omitted the sanitized Kimi unavailable state"
    grep -F -q 'Fable only' "$RUN_ROOT/codexbar-partial-once.txt" \
        || fail "--once omitted a Claude extra quota window"
    grep -F -q 'Tertiary' "$RUN_ROOT/codexbar-partial-once.txt" \
        || fail "--once omitted a Claude tertiary quota window"
    grep -F -q 'Codex Spark Weekly' "$RUN_ROOT/codexbar-partial-once.txt" \
        || fail "--once omitted a Codex extra quota window"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-partial-once.txt"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-partial-once.stderr.txt"
    kill_atui_session "$persisted_id"

    log "CodexBar: asserting sanitized unavailable state"
    printf '%s\n' error >"$mode_file"
    start_codexbar_abtop "codexbar-error" "$home" "$isolated_codex_home" "$fake_bin" "$mode_file" "$calls_file"
    error_id="$LAST_ATUI_SESSION"
    open_codexbar_config "$error_id" "codexbar-error"
    wait_screen_regex "$error_id" "${CONFIG_LABEL}[[:space:]]+unavailable([[:space:]]|$)" 20 "codexbar-unavailable-config" \
        || fail "CodexBar process failure was not surfaced as unavailable"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-unavailable-config.txt"
    env \
        "HOME=$home" \
        "XDG_CONFIG_HOME=$home/.config" \
        "CODEX_HOME=$isolated_codex_home" \
        "PATH=$fake_bin:$PATH" \
        "ABTOP_E2E_CODEXBAR_MODE_FILE=$mode_file" \
        "ABTOP_E2E_CODEXBAR_CALLS_FILE=$calls_file" \
        "$ABTOP_BIN" --json \
        >"$RUN_ROOT/codexbar-error-snapshot.json" \
        2>"$RUN_ROOT/codexbar-error-snapshot.stderr.txt"
    jq -e '
        .codexbar_quota.enabled == true and
        .codexbar_quota.state == "unavailable" and
        .codexbar_quota.provenance == null and
        .codexbar_quota.error == "process_failed"
    ' "$RUN_ROOT/codexbar-error-snapshot.json" >/dev/null \
        || fail "CodexBar error JSON snapshot was not sanitized and actionable"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-error-snapshot.json"
    assert_codexbar_private_data_absent "$RUN_ROOT/codexbar-error-snapshot.stderr.txt"

    log "CodexBar: asserting interactive disable and disabled persistence"
    # open_codexbar_config leaves the exact CodexBar row selected.
    atui "$error_id" press Enter >/dev/null
    wait_screen_regex "$error_id" "${CONFIG_LABEL}[[:space:]]+off([[:space:]]|$)" 10 "codexbar-disabled-config" \
        || fail "CodexBar setting did not switch off"
    grep -E -q '^codexbar_quota_fallback[[:space:]]*=[[:space:]]*false$' "$config_file" \
        || fail "CodexBar disabled setting was not persisted"
    kill_atui_session "$error_id"

    calls_before="$(wc -l <"$calls_file" | tr -d ' ')"
    start_codexbar_abtop "codexbar-off" "$home" "$isolated_codex_home" "$fake_bin" "$mode_file" "$calls_file"
    off_id="$LAST_ATUI_SESSION"
    open_codexbar_config "$off_id" "codexbar-off-persisted"
    assert_screen_regex "codexbar-off-persisted-selected" "${CONFIG_LABEL}[[:space:]]+off"
    calls_after="$(wc -l <"$calls_file" | tr -d ' ')"
    [[ "$calls_after" == "$calls_before" ]] \
        || fail "disabled persisted setting still invoked CodexBar"
    kill_atui_session "$off_id"
    log "CodexBar suite passed"
}

herdr_socket() {
    HERDR_SOCKET_PATH="$HERDR_SOCKET" herdr "$@"
}

wait_for_named_herdr() {
    local deadline sessions_json matches
    deadline=$(( $(date +%s) + 20 ))
    while :; do
        sessions_json="$(herdr session list --json)"
        matches="$(printf '%s' "$sessions_json" | jq --arg name "$HERDR_NAME" '[.sessions[] | select(.name == $name and .running == true)] | length')"
        if [[ "$matches" == 1 ]]; then
            HERDR_SOCKET="$(printf '%s' "$sessions_json" | jq -er --arg name "$HERDR_NAME" '.sessions[] | select(.name == $name and .running == true) | .socket_path')"
            [[ "$HERDR_SOCKET" == /* ]] || fail "named Herdr session returned a non-absolute socket"
            printf '%s\n' "$sessions_json" >"$RUN_ROOT/herdr-session-list.json"
            return 0
        fi
        (($(date +%s) < deadline)) || return 1
        sleep 1
    done
}

filtered_agent_list() {
    local output_file="$1"
    herdr_socket agent list | jq '{
        result: {
            agents: ([.result.agents[] |
                {
                    terminal_id, agent, name, agent_status,
                    agent_session, workspace_id, tab_id, pane_id, focused,
                    state_change_seq, revision
                } +
                if has("screen_detection_skipped") then
                    {screen_detection_skipped}
                else
                    {}
                end
            ] | sort_by([.terminal_id, .pane_id, (.agent_session.value // "")]))
        }
    }' >"$output_file"
}

filtered_process_info() {
    local pane_id="$1"
    local output_file="$2"
    herdr_socket pane process-info --pane "$pane_id" | jq '{
        result: {
            process_info: {
                pane_id: .result.process_info.pane_id,
                shell_pid: (.result.process_info.shell_pid // null),
                foreground_process_group_id:
                    (.result.process_info.foreground_process_group_id // null),
                foreground_processes:
                    ([.result.process_info.foreground_processes[] | {pid, name}]
                    | sort_by(.pid))
            }
        }
    }' >"$output_file"
}

wait_for_exact_codex_session() {
    local pane_id="$1"
    local agent_target="$2"
    local output_file="$3"
    local timeout_seconds="$4"
    local deadline matches
    deadline=$(( $(date +%s) + timeout_seconds ))
    while :; do
        filtered_agent_list "$output_file"
        matches="$(jq --arg pane "$pane_id" --arg target "$agent_target" '[
            .result.agents[] |
            select(
                .pane_id == $pane and
                .agent == "codex" and
                .name == $target and
                .agent_session.source == "herdr:codex" and
                .agent_session.agent == "codex" and
                .agent_session.kind == "id" and
                (.agent_session.value | type == "string" and length > 0 and length <= 512)
            )
        ] | length' "$output_file")"
        [[ "$matches" == 1 ]] && return 0
        (($(date +%s) < deadline)) || return 1
        sleep 1
    done
}

wait_agent_focused() {
    local target="$1"
    local deadline
    deadline=$(( $(date +%s) + 10 ))
    while :; do
        if herdr_socket agent get "$target" 2>/dev/null \
            | jq -e '.result.agent.focused == true' >/dev/null; then
            return 0
        fi
        (($(date +%s) < deadline)) || return 1
        sleep 1
    done
}

exact_codex_agent_pair_is_unchanged() {
    local before_file="$1"
    local after_file="$2"
    local pane_id="$3"
    local session_id="$4"
    local expected_status="$5"
    local agent_target="$6"

    jq -e \
        --slurpfile before "$before_file" \
        --arg pane "$pane_id" \
        --arg sid "$session_id" \
        --arg expected "$expected_status" \
        --arg target "$agent_target" '
            def exact_target($document):
                [$document.result.agents[] |
                    select(
                        .pane_id == $pane and
                        .agent == "codex" and
                        .name == $target and
                        .agent_session.source == "herdr:codex" and
                        .agent_session.agent == "codex" and
                        .agent_session.kind == "id" and
                        .agent_session.value == $sid
                    )];
            def screen_detection_used:
                ((has("screen_detection_skipped") | not) or
                    .screen_detection_skipped == false);
            exact_target($before[0]) as $first |
            exact_target(.) as $second |
            ($first | length) == 1 and
            ($second | length) == 1 and
            $first[0].agent_status == $expected and
            $second[0].agent_status == $expected and
            ($first[0] | screen_detection_used) and
            ($second[0] | screen_detection_used) and
            ($first[0].terminal_id | type == "string" and length > 0) and
            ($first[0].workspace_id | type == "string" and length > 0) and
            ($first[0].tab_id | type == "string" and length > 0) and
            ($first[0].state_change_seq | type == "number") and
            ($first[0].revision | type == "number") and
            $first[0] == $second[0]
        ' "$after_file" >/dev/null
}

codex_agent_explain_matches() {
    local explain_file="$1"
    local expected_status="$2"

    jq -e --arg expected "$expected_status" '
        def false_or_absent($field):
            ((has($field) | not) or .[$field] == false);
        .agent == "codex" and
        .state == $expected and
        (.manifest_source | type == "string" and length > 0) and
        (.manifest_version | type == "string" and length > 0) and
        (.matched_rule | type == "object") and
        .matched_rule.state == $expected and
        (.matched_rule.id | type == "string" and length > 0) and
        (.matched_rule.region | type == "string" and length > 0) and
        false_or_absent("screen_detection_skipped") and
        ((has("skipped_update_reason") | not) or
            .skipped_update_reason == null) and
        ((has("skip_state_update") | not) or .skip_state_update == false)
    ' "$explain_file" >/dev/null
}

start_codex_agent() {
    local agent_target="$1"
    local pane_id="$2"
    local output_file="$3"
    local error_prefix="$4"
    shift 4
    local start_output start_status start_deadline start_attempts
    local start_timeout_ms start_remaining_seconds

    start_deadline=$(( $(date +%s) + 20 ))
    start_attempts=0
    while :; do
        start_attempts=$((start_attempts + 1))
        start_remaining_seconds=$((start_deadline - $(date +%s)))
        if ((start_remaining_seconds <= 3)); then
            printf '%s\n' \
                "Codex start deadline expired before attempt $start_attempts" \
                >"$RUN_ROOT/$error_prefix.txt"
            herdr_socket pane process-info --pane "$pane_id" \
                >"$RUN_ROOT/$error_prefix-process-info.json" 2>&1 || true
            capture_screen "$HERDR_TUI_SESSION" "$error_prefix-screen" || true
            return 1
        fi
        start_timeout_ms=$((start_remaining_seconds * 1000))
        # Herdr requires more than 3000 ms and one successful launch may need
        # its entire readiness window. Busy-shell retries return immediately;
        # every other attempt is bounded by the remaining 20-second deadline.
        set +e
        start_output="$(herdr_socket agent start "$agent_target" \
            --kind codex --pane "$pane_id" --timeout "$start_timeout_ms" -- \
            "$@" 2>&1)"
        start_status=$?
        set -e
        if ((start_status == 0)); then
            if printf '%s\n' "$start_output" | jq -e \
                --arg name "$agent_target" --arg pane "$pane_id" '
                    .id == "cli:agent:start" and
                    .result.type == "agent_started" and
                    .result.agent.agent == "codex" and
                    .result.agent.name == $name and
                    .result.agent.pane_id == $pane and
                    .result.agent.interactive_ready == true
                ' >/dev/null 2>&1; then
                printf '%s\n' "$start_output" >"$output_file"
                return 0
            fi
            printf '%s\n' "$start_output" >"$RUN_ROOT/$error_prefix.txt"
            herdr_socket pane process-info --pane "$pane_id" \
                >"$RUN_ROOT/$error_prefix-process-info.json" 2>&1 || true
            capture_screen "$HERDR_TUI_SESSION" "$error_prefix-screen" || true
            return 1
        fi

        if ! printf '%s\n' "$start_output" \
            | jq -e '.error.code == "agent_pane_busy"' >/dev/null 2>&1 \
            || (($(date +%s) >= start_deadline)); then
            printf '%s\n' "$start_output" >"$RUN_ROOT/$error_prefix.txt"
            herdr_socket pane process-info --pane "$pane_id" \
                >"$RUN_ROOT/$error_prefix-process-info.json" 2>&1 || true
            capture_screen "$HERDR_TUI_SESSION" "$error_prefix-screen" || true
            return 1
        fi
        sleep 1
    done
}

wait_for_pid_exit() {
    local pid="$1"
    local timeout_seconds="$2"
    local deadline
    deadline=$(( $(date +%s) + timeout_seconds ))
    while kill -0 "$pid" 2>/dev/null; do
        (($(date +%s) < deadline)) || return 1
        sleep 1
    done
}

wait_for_pane_process_absent() {
    local pane_id="$1"
    local process_name="$2"
    local timeout_seconds="$3"
    local deadline process_file
    deadline=$(( $(date +%s) + timeout_seconds ))
    process_file="$RUN_ROOT/pane-process-absence-$pane_id.json"
    while :; do
        if filtered_process_info "$pane_id" "$process_file" \
            && jq -e --arg name "$process_name" '
                all(.result.process_info.foreground_processes[];
                    .name != $name)
            ' "$process_file" >/dev/null; then
            return 0
        fi
        (($(date +%s) < deadline)) || return 1
        sleep 1
    done
}

wait_for_pane_process_present() {
    local pane_id="$1"
    local process_name="$2"
    local timeout_seconds="$3"
    local deadline process_file
    deadline=$(( $(date +%s) + timeout_seconds ))
    process_file="$RUN_ROOT/pane-process-presence-$pane_id.json"
    while :; do
        if filtered_process_info "$pane_id" "$process_file" \
            && jq -e --arg name "$process_name" '
                any(.result.process_info.foreground_processes[];
                    .name == $name)
            ' "$process_file" >/dev/null; then
            return 0
        fi
        (($(date +%s) < deadline)) || return 1
        sleep 1
    done
}

wait_for_fresh_snapshot_without_session() {
    local session_id="$1"
    local status_home="$2"
    local timeout_seconds="$3"
    local evidence_name="$4"
    local deadline attempt snapshot_file
    deadline=$(( $(date +%s) + timeout_seconds ))
    attempt=0
    while :; do
        attempt=$((attempt + 1))
        printf -v snapshot_file '%s/%s-poll-%03d.json' \
            "$RUN_ROOT" "$evidence_name" "$attempt"
        env \
            "HOME=$status_home" \
            "XDG_CONFIG_HOME=$status_home/.config" \
            "CODEX_HOME=$CODEX_HOME_PATH" \
            HERDR_ENV=1 \
            "HERDR_SOCKET_PATH=$HERDR_SOCKET" \
            "$ABTOP_BIN" --json >"$snapshot_file"
        if jq -e --arg sid "$session_id" '
            all(.sessions[]; .session_id != $sid)
        ' "$snapshot_file" >/dev/null; then
            cp "$snapshot_file" "$RUN_ROOT/$evidence_name.json"
            return 0
        fi
        (($(date +%s) < deadline)) || return 1
        sleep 1
    done
}

capture_codex_status_phase() {
    local phase="$1"
    local expected_status="$2"
    local expected_reason="$3"
    local expected_herdr_status="$4"
    local status_home="$5"
    local root_pane="$6"
    local codex_agent_target="$7"
    local codex_session_id="$8"
    local timeout_seconds="$9"
    local screen_evidence_name="${10}"
    local deadline attempt now poll_base before_process_file after_process_file
    local before_explain_file after_explain_file before_snapshot_file
    local after_snapshot_file after_screen_file explain_file process_file
    local snapshot_file screen_file native_pid codex_display_id ui_label ui_regex

    case "$expected_status" in
        Idle) ui_label="Idle" ;;
        Executing) ui_label="Exec" ;;
        Working) ui_label="Work" ;;
        *) return 1 ;;
    esac
    # The table deliberately renders the first UUID group, while the selected
    # session detail and filter retain the complete native UUID.
    codex_display_id="${codex_session_id%%-*}"
    ui_regex="($codex_display_id.*$ui_label|$ui_label.*$codex_display_id)"
    deadline=$(( $(date +%s) + timeout_seconds ))
    attempt=0

    while :; do
        attempt=$((attempt + 1))
        printf -v poll_base 'codex-%s-poll-%03d' "$phase" "$attempt"
        before_process_file="$RUN_ROOT/$poll_base-herdr-before-process.json"
        after_process_file="$RUN_ROOT/$poll_base-herdr-after-process.json"
        before_explain_file="$RUN_ROOT/$poll_base-herdr-before-explain.json"
        after_explain_file="$RUN_ROOT/$poll_base-herdr-after-explain.json"
        before_snapshot_file="$RUN_ROOT/$poll_base-herdr-before-snapshot.json"
        after_snapshot_file="$RUN_ROOT/$poll_base-herdr-after-snapshot.json"
        after_screen_file="$RUN_ROOT/$poll_base-herdr-after-screen.json"
        explain_file="$RUN_ROOT/$poll_base-herdr-explain.json"
        process_file="$RUN_ROOT/$poll_base-process-info.json"
        snapshot_file="$RUN_ROOT/$poll_base-snapshot.json"
        screen_file="$RUN_ROOT/$poll_base-screen.txt"

        # Each provider read is independently bracketed by one unchanged exact
        # Herdr row. A changing revision, identity, pane, session, or state
        # invalidates the attempt instead of being normalized away.
        if filtered_agent_list "$before_process_file" \
            && filtered_process_info "$root_pane" "$process_file" \
            && filtered_agent_list "$after_process_file" \
            && exact_codex_agent_pair_is_unchanged \
                "$before_process_file" "$after_process_file" \
                "$root_pane" "$codex_session_id" "$expected_herdr_status" \
                "$codex_agent_target" \
            && filtered_agent_list "$before_explain_file" \
            && herdr_socket agent explain "$codex_agent_target" --json \
                >"$explain_file" \
            && filtered_agent_list "$after_explain_file" \
            && exact_codex_agent_pair_is_unchanged \
                "$before_explain_file" "$after_explain_file" \
                "$root_pane" "$codex_session_id" "$expected_herdr_status" \
                "$codex_agent_target" \
            && codex_agent_explain_matches \
                "$explain_file" "$expected_herdr_status" \
            && filtered_agent_list "$before_snapshot_file" \
            && env \
                "HOME=$status_home" \
                "XDG_CONFIG_HOME=$status_home/.config" \
                "CODEX_HOME=$CODEX_HOME_PATH" \
                HERDR_ENV=1 \
                "HERDR_SOCKET_PATH=$HERDR_SOCKET" \
                "$ABTOP_BIN" --json >"$snapshot_file" \
            && filtered_agent_list "$after_snapshot_file" \
            && exact_codex_agent_pair_is_unchanged \
                "$before_snapshot_file" "$after_snapshot_file" \
                "$root_pane" "$codex_session_id" "$expected_herdr_status" \
                "$codex_agent_target" \
            && capture_screen "$HERDR_TUI_SESSION" "$poll_base-screen" \
            && filtered_agent_list "$after_screen_file" \
            && exact_codex_agent_pair_is_unchanged \
                "$after_snapshot_file" "$after_screen_file" \
                "$root_pane" "$codex_session_id" "$expected_herdr_status" \
                "$codex_agent_target" \
            && exact_codex_agent_pair_is_unchanged \
                "$before_process_file" "$after_screen_file" \
                "$root_pane" "$codex_session_id" "$expected_herdr_status" \
                "$codex_agent_target"; then
            native_pid="$(jq -er '
                [.result.process_info.foreground_processes[] |
                    select(
                        .name == "codex" and
                        (.pid | type == "number" and . > 0)
                    )]
                | if length == 1 then .[0].pid
                  else error("expected one native Codex process") end
            ' "$process_file" 2>/dev/null || true)"

            if [[ -n "$native_pid" ]] \
                && jq -e --arg pane "$root_pane" --argjson pid "$native_pid" '
                    .result.process_info.pane_id == $pane and
                    ([.result.process_info.foreground_processes[] |
                        select(
                            .pid == $pid and
                            .name == "codex"
                        )] | length) == 1
                    ' "$process_file" >/dev/null \
                && jq -e \
                    --arg sid "$codex_session_id" \
                    --argjson pid "$native_pid" \
                    --arg status "$expected_status" \
                    --arg reason "$expected_reason" '
                        [.sessions[] |
                            select(.agent_cli == "codex" and .session_id == $sid)] as $rows |
                        ($rows | length) == 1 and
                        $rows[0].pid == $pid and
                        $rows[0].status == $status and
                        $rows[0].status_evidence.authority == "Heuristic" and
                        $rows[0].status_evidence.reason == $reason and
                        $rows[0].status_evidence.connection_generation == 0 and
                        $rows[0].status_evidence.observations[-1].status == $status and
                        $rows[0].status_evidence.observations[-1].reason == $reason
                    ' "$snapshot_file" >/dev/null \
                && grep -E -q "$ui_regex" "$screen_file" \
                && grep -F -q "$codex_session_id" "$screen_file"; then
                if jq -n \
                        --arg pane_id "$root_pane" \
                        --arg session_id "$codex_session_id" \
                        --argjson native_codex_pid "$native_pid" \
                        '{pane_id: $pane_id,
                          session_id: $session_id,
                          native_codex_pid: $native_codex_pid}' \
                        >"$RUN_ROOT/codex-$phase-correlation.json" \
                    && cp "$before_process_file" \
                        "$RUN_ROOT/codex-$phase-herdr-agent-before-process.json" \
                    && cp "$after_process_file" \
                        "$RUN_ROOT/codex-$phase-herdr-agent-after-process.json" \
                    && cp "$before_explain_file" \
                        "$RUN_ROOT/codex-$phase-herdr-agent-before-explain.json" \
                    && cp "$after_explain_file" \
                        "$RUN_ROOT/codex-$phase-herdr-agent-after-explain.json" \
                    && cp "$before_snapshot_file" \
                        "$RUN_ROOT/codex-$phase-herdr-agent-before-snapshot.json" \
                    && cp "$after_snapshot_file" \
                        "$RUN_ROOT/codex-$phase-herdr-agent-after-snapshot.json" \
                    && cp "$after_screen_file" \
                        "$RUN_ROOT/codex-$phase-herdr-agent-after-screen.json" \
                    && cp "$after_screen_file" \
                        "$RUN_ROOT/codex-$phase-herdr-agent.json" \
                    && cp "$explain_file" \
                        "$RUN_ROOT/codex-$phase-herdr-explain.json" \
                    && cp "$process_file" \
                        "$RUN_ROOT/codex-$phase-process-info.json" \
                    && cp "$snapshot_file" \
                        "$RUN_ROOT/codex-$phase-snapshot.json" \
                    && cp "$RUN_ROOT/$poll_base-screen.json" \
                        "$RUN_ROOT/$screen_evidence_name.json" \
                    && cp "$screen_file" \
                        "$RUN_ROOT/$screen_evidence_name.txt"; then
                    return 0
                fi
                return 1
            fi
        fi

        now=$(date +%s)
        ((now < deadline)) || return 1
        sleep 1
    done
}

run_codex_status_suite() {
    local suite_root status_home session_list existing_count root_pane split_json abtop_pane
    local codex_agent_target codex_session_id hook_native_pid
    local abtop_command abtop_run_output current_pane filter_snapshot_file
    local negative_filter_id negative_filter_last negative_filter_replacement
    local no_hooks_split_json no_hooks_pane no_hooks_agent_target
    local no_hooks_session_id no_hooks_native_pid no_hooks_focus_pane
    require_command herdr
    require_command codex
    require_command git

    suite_root="$RUN_ROOT/codex-status"
    status_home="$suite_root/home"
    mkdir -p "$status_home"
    git -C "$WORKSPACE_PATH" rev-parse --show-toplevel >"$RUN_ROOT/codex-workspace-root.txt" \
        || fail "--workspace must be inside an existing trusted Git workspace"

    if ! env CODEX_HOME="$CODEX_HOME_PATH" "$ABTOP_BIN" --codex-integration-status \
        >"$RUN_ROOT/codex-integration-status.txt" 2>&1; then
        fail "Codex integration is not already healthy; review only the 11 abtop hooks, then rerun"
    fi
    codex --version >"$RUN_ROOT/codex-version.txt"
    herdr --version >"$RUN_ROOT/herdr-version.txt"

    HERDR_NAME="abtop-e2e-$(date +%s)-$$"
    session_list="$(herdr session list --json)"
    existing_count="$(printf '%s' "$session_list" | jq --arg name "$HERDR_NAME" '[.sessions[] | select(.name == $name)] | length')"
    [[ "$existing_count" == 0 ]] || fail "refusing to reuse existing Herdr session $HERDR_NAME"

    log "Codex status: creating exact named Herdr session $HERDR_NAME"
    # agent-tui creates a separate virtual terminal. Do not let an outer Herdr
    # pane's inherited runtime identity misclassify that PTY as a nested launch.
    run_atui_session "$RUN_ROOT/herdr-agent-tui-run.json" \
        --cols 160 --rows 48 --cwd "$WORKSPACE_PATH" -- \
        env \
        -u HERDR_ENV \
        -u HERDR_SOCKET_PATH \
        -u HERDR_CLIENT_SOCKET_PATH \
        -u HERDR_SESSION \
        -u HERDR_PANE_ID \
        -u HERDR_TAB_ID \
        -u HERDR_WORKSPACE_ID \
        -u HERDR_STARTUP_CWD \
        "CODEX_HOME=$CODEX_HOME_PATH" \
        herdr --session "$HERDR_NAME"
    HERDR_TUI_SESSION="$LAST_ATUI_SESSION"
    HERDR_MAY_EXIST=1
    if ! wait_for_named_herdr; then
        herdr session list --json >"$RUN_ROOT/herdr-session-list-failed.json" 2>&1 || true
        capture_screen "$HERDR_TUI_SESSION" "herdr-session-start-failed" || true
        fail "named Herdr session did not become available"
    fi

    herdr_socket pane list >"$RUN_ROOT/herdr-panes-initial.json"
    root_pane="$(jq -er '.result.panes | if length == 1 then .[0].pane_id else error("expected exactly one initial pane") end' "$RUN_ROOT/herdr-panes-initial.json")"
    split_json="$(herdr_socket pane split "$root_pane" --direction down --ratio 0.5 --cwd "$WORKSPACE_PATH" --no-focus)"
    printf '%s\n' "$split_json" >"$RUN_ROOT/herdr-pane-split.json"
    abtop_pane="$(printf '%s' "$split_json" | jq -er '.result.pane.pane_id')"
    [[ "$abtop_pane" != "$root_pane" ]] || fail "Herdr split reused the root pane"

    log "Codex status: starting authenticated Codex with read-only sandbox"
    codex_agent_target="abtop-e2e-codex"
    start_codex_agent \
        "$codex_agent_target" "$root_pane" \
        "$RUN_ROOT/codex-agent-start.txt" "codex-agent-start-error" \
        --sandbox read-only --ask-for-approval never --no-alt-screen \
        -C "$WORKSPACE_PATH" \
        || fail "Codex did not reach ready state; no hook or directory trust was changed"

    # Codex queues its startup SessionStart hook into the first turn. Run one
    # bounded no-tool turn so Herdr can publish the exact native session ID.
    log "Codex status: establishing exact native session identity"
    herdr_socket agent prompt "$codex_agent_target" \
        'Reply with READY. Do not run any tools.' \
        --wait --until idle --until "done" --timeout 120000 \
        >"$RUN_ROOT/herdr-codex-identity-prompt.json"
    herdr_socket agent focus "$codex_agent_target" \
        >"$RUN_ROOT/herdr-focus-codex-after-identity.json"
    herdr_socket agent wait "$codex_agent_target" --until idle --timeout 10000 \
        >"$RUN_ROOT/herdr-wait-identity-idle.json"
    wait_for_exact_codex_session \
        "$root_pane" "$codex_agent_target" \
        "$RUN_ROOT/herdr-agents-ready.json" 20 \
        || fail "Herdr did not publish one exact native Codex session reference"
    codex_session_id="$(jq -er --arg pane "$root_pane" '
        [.result.agents[] |
            select(
                .pane_id == $pane and
                .agent == "codex" and
                .name == "abtop-e2e-codex" and
                .agent_session.source == "herdr:codex" and
                .agent_session.agent == "codex" and
                .agent_session.kind == "id"
            )]
        | if length == 1 then .[0].agent_session.value else error("expected one exact Codex session") end
    ' "$RUN_ROOT/herdr-agents-ready.json")"
    [[ "$codex_session_id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
        || fail "Herdr returned a non-UUID Codex session reference"
    printf -v abtop_command \
        'env HOME=%q XDG_CONFIG_HOME=%q CODEX_HOME=%q HERDR_SOCKET_PATH=%q %q --exit-on-jump' \
        "$status_home" "$status_home/.config" "$CODEX_HOME_PATH" "$HERDR_SOCKET" "$ABTOP_BIN"
    abtop_run_output="$(herdr_socket pane run "$abtop_pane" "$abtop_command")"
    printf '%s\n' "$abtop_run_output" >"$RUN_ROOT/herdr-abtop-pane-run.json"
    wait_screen_regex "$HERDR_TUI_SESSION" "abtop v[0-9]" 20 "codex-status-abtop-ready" \
        || fail "abtop did not render before the UI input phase"

    # Focus the exact abtop pane, observe it, then filter with the complete
    # native Codex UUID. The footer truncates the displayed filter, so result
    # uniqueness is proved with a tail-sensitive negative filter, exact JSON
    # identity, and navigation invariance.
    herdr_socket pane focus --direction down --pane "$root_pane" \
        >"$RUN_ROOT/herdr-focus-abtop.json"
    current_pane="$(jq -er '.result.focus.focused_pane_id' "$RUN_ROOT/herdr-focus-abtop.json")"
    [[ "$current_pane" == "$abtop_pane" ]] || fail "Herdr did not focus the exact abtop pane"
    # One raw write makes the filter text and CR indivisible. A literal LF is
    # Ctrl+J in a raw terminal and must never be used as Enter.
    herdr_socket pane send-text "$abtop_pane" $'/'"$codex_session_id"$'\r' \
        >"$RUN_ROOT/herdr-filter-abtop.json"
    wait_screen_regex \
        "$HERDR_TUI_SESSION" \
        "SESSION \(►$codex_session_id" \
        20 \
        "codex-status-filtered" \
        || fail "abtop did not select the exact full-UUID Codex filter result"
    wait_codex_table_row_count \
        "$HERDR_TUI_SESSION" 1 10 "codex-status-filtered-one-row" \
        || fail "the filtered Sessions table did not render exactly one Codex row"
    filter_snapshot_file="$RUN_ROOT/codex-filter-snapshot.json"
    env \
        "HOME=$status_home" \
        "XDG_CONFIG_HOME=$status_home/.config" \
        "CODEX_HOME=$CODEX_HOME_PATH" \
        HERDR_ENV=1 \
        "HERDR_SOCKET_PATH=$HERDR_SOCKET" \
        "$ABTOP_BIN" --json >"$filter_snapshot_file"
    jq -e --arg sid "$codex_session_id" '
        [.sessions[] | select(.agent_cli == "codex" and .session_id == $sid)] |
        length == 1
    ' "$filter_snapshot_file" >/dev/null \
        || fail "the full Codex UUID did not identify exactly one JSON session"

    # Change only a non-rendered UUID-tail nibble. If the TUI consumed merely
    # the displayed first group, this negative filter would incorrectly retain
    # the Codex row and the zero-row assertion would fail.
    negative_filter_last="${codex_session_id: -1}"
    negative_filter_replacement="0"
    if [[ "$negative_filter_last" == "0" ]]; then
        negative_filter_replacement="1"
    fi
    negative_filter_id="${codex_session_id%?}$negative_filter_replacement"
    printf '%s\n' "$negative_filter_id" \
        >"$RUN_ROOT/codex-filter-tail-negative-value.txt"
    herdr_socket pane send-keys "$abtop_pane" esc \
        >"$RUN_ROOT/herdr-filter-clear-before-negative.json"
    herdr_socket pane send-text "$abtop_pane" $'/'"$negative_filter_id"$'\r' \
        >"$RUN_ROOT/herdr-filter-tail-negative.json"
    wait_codex_table_row_count \
        "$HERDR_TUI_SESSION" 0 10 "codex-status-filter-tail-negative" \
        || fail "abtop ignored the non-rendered suffix of its full-UUID filter"

    herdr_socket pane send-keys "$abtop_pane" esc \
        >"$RUN_ROOT/herdr-filter-clear-before-restore.json"
    herdr_socket pane send-text "$abtop_pane" $'/'"$codex_session_id"$'\r' \
        >"$RUN_ROOT/herdr-filter-restore.json"
    wait_codex_table_row_count \
        "$HERDR_TUI_SESSION" 1 10 "codex-status-filter-restored" \
        || fail "abtop did not restore the one-row full-UUID filter"
    wait_screen_regex \
        "$HERDR_TUI_SESSION" "SESSION \(►$codex_session_id" 10 \
        "codex-status-filter-restored-session" \
        || fail "abtop did not restore the exact full-UUID Codex selection"
    herdr_socket pane send-keys "$abtop_pane" down \
        >"$RUN_ROOT/herdr-filter-down.json"
    wait_screen_regex \
        "$HERDR_TUI_SESSION" "SESSION \(►$codex_session_id" 3 \
        "codex-status-filter-down" \
        || fail "Down changed the only full-UUID Codex filter result"
    herdr_socket pane send-keys "$abtop_pane" up \
        >"$RUN_ROOT/herdr-filter-up.json"
    wait_screen_regex \
        "$HERDR_TUI_SESSION" "SESSION \(►$codex_session_id" 3 \
        "codex-status-one-match" \
        || fail "Up changed the only full-UUID Codex filter result"

    log "Codex status: asserting idle, live root tool execution, and idle completion"
    herdr_socket agent wait "$codex_agent_target" --until idle --timeout 60000 \
        >"$RUN_ROOT/herdr-wait-initial-idle.json"
    capture_codex_status_phase \
        initial-idle Idle HerdrScreenIdle idle \
        "$status_home" "$root_pane" "$codex_agent_target" "$codex_session_id" \
        20 codex-status-idle \
        || fail "abtop did not project one bracketed exact idle state"
    herdr_socket agent prompt "$codex_agent_target" \
        'Run the shell command sleep 30. After it finishes, reply briefly.' \
        >"$RUN_ROOT/herdr-codex-tool-prompt.json"
    herdr_socket agent wait "$codex_agent_target" --until working --timeout 30000 \
        >"$RUN_ROOT/herdr-wait-working.json"
    capture_codex_status_phase \
        executing Executing HerdrScreenWorking working \
        "$status_home" "$root_pane" "$codex_agent_target" "$codex_session_id" \
        60 codex-status-executing \
        || fail "abtop did not show one bracketed Exec phase for the exact root tool"
    herdr_socket agent wait "$codex_agent_target" --until idle --timeout 120000 \
        >"$RUN_ROOT/herdr-wait-final-idle.json"
    capture_codex_status_phase \
        final-idle Idle HerdrScreenIdle idle \
        "$status_home" "$root_pane" "$codex_agent_target" "$codex_session_id" \
        30 codex-status-complete \
        || fail "abtop did not return the exact Codex session to a bracketed Idle phase"

    log "Codex status: asserting Enter focuses the exact Codex pane"
    herdr_socket pane focus --direction down --pane "$root_pane" \
        >"$RUN_ROOT/herdr-refocus-abtop.json"
    herdr_socket pane list >"$RUN_ROOT/herdr-panes-before-enter.json"
    herdr_socket agent get "$root_pane" >"$RUN_ROOT/herdr-agent-before-enter.json"
    jq -e --arg pane "$abtop_pane" '
        [.result.panes[] | select(.focused == true and .pane_id == $pane)] | length == 1
    ' "$RUN_ROOT/herdr-panes-before-enter.json" >/dev/null \
        || fail "the exact abtop pane was not focused before Enter"
    capture_screen "$HERDR_TUI_SESSION" "codex-focus-before-enter"
    assert_screen_absent "codex-focus-before-enter" "agent list returned invalid JSON"
    atui "$HERDR_TUI_SESSION" press Enter >"$RUN_ROOT/agent-tui-abtop-enter.json"
    wait_agent_focused "$root_pane" || fail "Enter did not focus the exact Codex pane"
    herdr_socket pane list >"$RUN_ROOT/herdr-panes-after-enter.json"
    capture_screen "$HERDR_TUI_SESSION" "codex-focus-after-enter"
    assert_screen_absent "codex-focus-after-enter" "agent list returned invalid JSON"
    herdr_socket agent get "$root_pane" >"$RUN_ROOT/herdr-focused-codex.json"
    jq -e --arg pane "$root_pane" '
        [.result.panes[] | select(.focused == true and .pane_id == $pane)] | length == 1
    ' "$RUN_ROOT/herdr-panes-after-enter.json" >/dev/null \
        || fail "Herdr pane state did not move focus to the exact Codex pane"
    jq -e \
        --arg pane "$root_pane" \
        --arg sid "$codex_session_id" '
            .result.agent.focused == true and
            .result.agent.pane_id == $pane and
            .result.agent.agent == "codex" and
            .result.agent.name == "abtop-e2e-codex" and
            .result.agent.agent_session.source == "herdr:codex" and
            .result.agent.agent_session.agent == "codex" and
            .result.agent.agent_session.kind == "id" and
            .result.agent.agent_session.value == $sid
        ' "$RUN_ROOT/herdr-focused-codex.json" >/dev/null \
        || fail "Herdr did not confirm exact Codex focus"

    hook_native_pid="$(jq -er '.native_codex_pid' \
        "$RUN_ROOT/codex-final-idle-correlation.json")"

    log "Codex status: asserting exact Herdr Working without abtop hooks"
    no_hooks_agent_target="abtop-e2e-codex-no-hooks"
    no_hooks_split_json="$(herdr_socket pane split "$root_pane" \
        --direction right --ratio 0.5 --cwd "$WORKSPACE_PATH" --no-focus)"
    printf '%s\n' "$no_hooks_split_json" \
        >"$RUN_ROOT/herdr-no-hooks-pane-split.json"
    no_hooks_pane="$(printf '%s' "$no_hooks_split_json" \
        | jq -er '.result.pane.pane_id')"
    [[ "$no_hooks_pane" != "$root_pane" && "$no_hooks_pane" != "$abtop_pane" ]] \
        || fail "Herdr reused a pane for the no-hooks Codex session"
    start_codex_agent \
        "$no_hooks_agent_target" "$no_hooks_pane" \
        "$RUN_ROOT/codex-no-hooks-agent-start.txt" \
        "codex-no-hooks-agent-start-error" \
        --config 'plugins."abtop@abtop-local".enabled=false' \
        --sandbox read-only --ask-for-approval never \
        --no-alt-screen -C "$WORKSPACE_PATH" \
        || fail "Codex with invocation-scoped disabled abtop plugin did not reach ready state"

    herdr_socket agent prompt "$no_hooks_agent_target" \
        'Reply with READY. Do not run any tools.' \
        --wait --until idle --until "done" --timeout 120000 \
        >"$RUN_ROOT/herdr-codex-no-hooks-identity-prompt.json"
    herdr_socket agent wait "$no_hooks_agent_target" --until idle --timeout 10000 \
        >"$RUN_ROOT/herdr-wait-no-hooks-identity-idle.json"
    wait_for_exact_codex_session \
        "$no_hooks_pane" "$no_hooks_agent_target" \
        "$RUN_ROOT/herdr-no-hooks-agents-ready.json" 20 \
        || fail "Herdr did not publish the no-hooks Codex session identity"
    no_hooks_session_id="$(jq -er \
        --arg pane "$no_hooks_pane" --arg target "$no_hooks_agent_target" '
            [.result.agents[] |
                select(
                    .pane_id == $pane and
                    .agent == "codex" and
                    .name == $target and
                    .agent_session.source == "herdr:codex" and
                    .agent_session.agent == "codex" and
                    .agent_session.kind == "id"
                )]
            | if length == 1 then .[0].agent_session.value
              else error("expected one exact no-hooks Codex session") end
        ' "$RUN_ROOT/herdr-no-hooks-agents-ready.json")"
    [[ "$no_hooks_session_id" =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
        || fail "Herdr returned a non-UUID no-hooks Codex session reference"

    wait_for_pane_process_absent "$abtop_pane" abtop 10 \
        || fail "the abtop pane did not return to its shell after exact focus"
    if ! abtop_run_output="$(herdr_socket pane run \
        "$abtop_pane" "$abtop_command")"; then
        fail "the abtop pane did not return to its shell after exact focus"
    fi
    printf '%s\n' "$abtop_run_output" \
        >"$RUN_ROOT/herdr-no-hooks-abtop-pane-run.json"
    wait_for_pane_process_present "$abtop_pane" abtop 10 \
        || fail "abtop did not become the foreground process for its second run"
    wait_screen_regex "$HERDR_TUI_SESSION" "abtop v[0-9]" 20 \
        "codex-no-hooks-abtop-ready" \
        || fail "abtop did not restart for the no-hooks status phase"
    herdr_socket pane send-text \
        "$abtop_pane" $'/'"$no_hooks_session_id"$'\r' \
        >"$RUN_ROOT/herdr-no-hooks-filter-abtop.json"
    wait_codex_table_row_count \
        "$HERDR_TUI_SESSION" 1 10 "codex-no-hooks-filtered-one-row" \
        || fail "the no-hooks full-UUID filter did not select exactly one row"
    wait_screen_regex \
        "$HERDR_TUI_SESSION" "SESSION \(►$no_hooks_session_id" 10 \
        "codex-no-hooks-filtered-session" \
        || fail "abtop did not select the exact no-hooks Codex session"

    herdr_socket agent prompt "$no_hooks_agent_target" \
        'Run the shell command sleep 30. After it finishes, reply briefly.' \
        >"$RUN_ROOT/herdr-codex-no-hooks-tool-prompt.json"
    herdr_socket agent wait "$no_hooks_agent_target" \
        --until working --timeout 30000 \
        >"$RUN_ROOT/herdr-wait-no-hooks-working.json"
    capture_codex_status_phase \
        no-hooks-working Working HerdrWorkingUnrefined working \
        "$status_home" "$no_hooks_pane" "$no_hooks_agent_target" \
        "$no_hooks_session_id" 60 codex-status-no-hooks-working \
        || fail "abtop did not show bracketed Work for exact Herdr activity without hooks"
    no_hooks_native_pid="$(jq -er '.native_codex_pid' \
        "$RUN_ROOT/codex-no-hooks-working-correlation.json")"

    log "Codex status: asserting Working remains non-PID-actionable"
    herdr_socket pane focus --direction down --pane "$no_hooks_pane" \
        >"$RUN_ROOT/herdr-focus-abtop-before-no-hooks-kill.json"
    no_hooks_focus_pane="$(jq -er '.result.focus.focused_pane_id' \
        "$RUN_ROOT/herdr-focus-abtop-before-no-hooks-kill.json")"
    [[ "$no_hooks_focus_pane" == "$abtop_pane" ]] \
        || fail "Herdr did not focus the exact abtop pane for the no-hooks row"
    atui "$HERDR_TUI_SESSION" press x \
        >"$RUN_ROOT/agent-tui-abtop-no-hooks-kill-attempt.json"
    capture_screen "$HERDR_TUI_SESSION" "codex-no-hooks-after-kill-attempt"
    assert_screen_absent \
        "codex-no-hooks-after-kill-attempt" "Press x again to kill"
    kill -0 "$no_hooks_native_pid" 2>/dev/null \
        || fail "a non-actionable Working row terminated its native Codex process"

    log "Codex status: asserting Enter semantically focuses Working"
    assert_screen_absent \
        "codex-no-hooks-after-kill-attempt" "agent list returned invalid JSON"
    atui "$HERDR_TUI_SESSION" press Enter \
        >"$RUN_ROOT/agent-tui-abtop-no-hooks-enter.json"
    wait_agent_focused "$no_hooks_pane" \
        || fail "Enter did not focus the exact no-hooks Codex pane"
    herdr_socket agent get "$no_hooks_pane" \
        >"$RUN_ROOT/herdr-focused-no-hooks-codex.json"
    jq -e \
        --arg pane "$no_hooks_pane" \
        --arg target "$no_hooks_agent_target" \
        --arg sid "$no_hooks_session_id" '
            .result.agent.focused == true and
            .result.agent.pane_id == $pane and
            .result.agent.agent == "codex" and
            .result.agent.name == $target and
            .result.agent.agent_session.source == "herdr:codex" and
            .result.agent.agent_session.agent == "codex" and
            .result.agent.agent_session.kind == "id" and
            .result.agent.agent_session.value == $sid
        ' "$RUN_ROOT/herdr-focused-no-hooks-codex.json" >/dev/null \
        || fail "Herdr did not confirm semantic focus for Working"

    log "Codex status: asserting fresh collection suppresses a disposed hook session"
    herdr_socket pane close "$no_hooks_pane" \
        >"$RUN_ROOT/herdr-no-hooks-pane-close.json"
    herdr_socket pane close "$root_pane" \
        >"$RUN_ROOT/herdr-hook-pane-close.json"
    wait_for_pid_exit "$no_hooks_native_pid" 20 \
        || fail "the exact no-hooks Codex process survived its pane disposal"
    wait_for_pid_exit "$hook_native_pid" 20 \
        || fail "the exact hook-backed Codex process survived its pane disposal"
    wait_for_fresh_snapshot_without_session \
        "$codex_session_id" "$status_home" 30 \
        "codex-disposed-hook-session-snapshot" \
        || fail "fresh JSON retained the disposed hook session as a ghost row"

    log "Codex status suite passed"
    log "Note: model-only Think, dynamic approval/Wait, and 30-second Done are retained as manual review cases because Herdr exposes no deterministic synthetic lifecycle injector for them."
}

main() {
    parse_args "$@"
    trap cleanup EXIT HUP INT TERM
    prepare_runtime
    case "$SUITE" in
        codexbar)
            run_codexbar_suite
            ;;
        codex-status)
            run_codex_status_suite
            ;;
        all)
            run_codexbar_suite
            run_codex_status_suite
            ;;
    esac
    log "requested suite completed successfully"
}

main "$@"
