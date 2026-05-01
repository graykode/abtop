#!/bin/bash
# abtop StatusLine hook for Claude Code
# Reads StatusLine JSON from stdin and writes rate limit data to a file.
#
# Install: add to ~/.claude/settings.json:
#   "statusLine": { "command": "/path/to/abtop-statusline.sh" }
#
# Or run: abtop --setup

# Read JSON from stdin
input=$(cat)

transcript_path=""
if command -v jq &>/dev/null; then
    transcript_path=$(echo "$input" | jq -r '.transcript_path // empty' 2>/dev/null)
fi
if [[ "$transcript_path" == */projects/* ]]; then
    CONFIG_DIR="${transcript_path%%/projects/*}"
else
    CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
fi
OUTPUT_FILE="$CONFIG_DIR/abtop-rate-limits.json"

# Extract rate_limits using python/jq/node (whichever is available)
if command -v python3 &>/dev/null; then
    echo "$input" | python3 -c "
import sys, json, time, os
try:
    data = json.load(sys.stdin)
    rl = data.get('rate_limits', {})
    if not rl:
        sys.exit(0)
    out = {'source': 'claude', 'updated_at': int(time.time())}
    session = rl.get('five_hour') or rl.get('session') or {}
    weekly = rl.get('seven_day') or rl.get('weekly') or {}
    if session:
        out['five_hour'] = {
            'used_percentage': session.get('used_percentage', 0),
            'resets_at': session.get('resets_at', 0)
        }
    if weekly:
        out['seven_day'] = {
            'used_percentage': weekly.get('used_percentage', 0),
            'resets_at': weekly.get('resets_at', 0)
        }
    if 'five_hour' not in out and 'seven_day' not in out:
        sys.exit(0)
    config_dir = None
    transcript_path = data.get('transcript_path')
    if transcript_path:
        p = os.path.abspath(transcript_path)
        parts = p.split(os.sep)
        if 'projects' in parts:
            idx = parts.index('projects')
            inferred = os.sep.join(parts[:idx])
            if inferred:
                config_dir = inferred
    if not config_dir:
        config_dir = os.environ.get('CLAUDE_CONFIG_DIR')
    if not config_dir:
        config_dir = os.path.join(os.path.expanduser('~'), '.claude')
    path = os.path.join(config_dir, 'abtop-rate-limits.json')
    os.makedirs(config_dir, exist_ok=True)
    tmp = path + '.tmp'
    with open(tmp, 'w') as f:
        json.dump(out, f)
    os.replace(tmp, path)
except Exception:
    pass
"
elif command -v jq &>/dev/null; then
    five_pct=$(echo "$input" | jq -r '.rate_limits.five_hour.used_percentage // .rate_limits.session.used_percentage // empty' 2>/dev/null)
    if [ -n "$five_pct" ]; then
        five_reset=$(echo "$input" | jq -r '.rate_limits.five_hour.resets_at // .rate_limits.session.resets_at // 0')
        seven_pct=$(echo "$input" | jq -r '.rate_limits.seven_day.used_percentage // .rate_limits.weekly.used_percentage // 0')
        seven_reset=$(echo "$input" | jq -r '.rate_limits.seven_day.resets_at // .rate_limits.weekly.resets_at // 0')
        now=$(date +%s)
        mkdir -p "$CONFIG_DIR"
        tmp="$OUTPUT_FILE.tmp"
        cat > "$tmp" <<EOF
{"source":"claude","updated_at":$now,"five_hour":{"used_percentage":$five_pct,"resets_at":$five_reset},"seven_day":{"used_percentage":$seven_pct,"resets_at":$seven_reset}}
EOF
        mv "$tmp" "$OUTPUT_FILE"
    fi
fi
