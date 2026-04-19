use serde_json::Value;
use std::fs;
use std::path::PathBuf;

const STATUSLINE_SCRIPT: &str = r#"#!/bin/bash
# abtop StatusLine hook — writes rate limit data for abtop to read.
# Installed by: abtop --setup
# Reads JSON from stdin with a 5s timeout, pipes it to python via stdin
# to avoid ARG_MAX limits on large payloads.
INPUT=""
while IFS= read -r -t 5 line || [ -n "$line" ]; do
    INPUT="${INPUT}${line}
"
done
[ -z "$INPUT" ] && exit 0
printf '%s' "$INPUT" | python3 -c "
import sys, json, time, os
data = json.load(sys.stdin)
rl = data.get('rate_limits')
if not rl:
    sys.exit(0)
out = {'source': 'claude', 'updated_at': int(time.time())}
fh = rl.get('five_hour')
if fh:
    out['five_hour'] = {'used_percentage': fh.get('used_percentage', 0), 'resets_at': fh.get('resets_at', 0)}
sd = rl.get('seven_day')
if sd:
    out['seven_day'] = {'used_percentage': sd.get('used_percentage', 0), 'resets_at': sd.get('resets_at', 0)}
config_dir = os.environ.get('CLAUDE_CONFIG_DIR', os.path.join(os.path.expanduser('~'), '.claude'))
with open(os.path.join(config_dir, 'abtop-rate-limits.json'), 'w') as f:
    json.dump(out, f)
" 2>/dev/null
"#;

fn claude_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".claude"))
}

fn script_path() -> PathBuf {
    claude_dir().join("abtop-statusline.sh")
}

fn settings_path() -> PathBuf {
    claude_dir().join("settings.json")
}

pub fn run_setup() {
    println!("abtop --setup: configuring Claude Code StatusLine hook\n");

    // Ensure ~/.claude directory exists
    let dir = claude_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("  ✗ failed to create {}: {}", dir.display(), e);
        std::process::exit(1);
    }

    // Step 1: Write the statusline script
    let script = script_path();
    match fs::write(&script, STATUSLINE_SCRIPT) {
        Ok(_) => {
            // chmod +x
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o700));
            }
            println!("  ✓ wrote {}", script.display());
        }
        Err(e) => {
            eprintln!("  ✗ failed to write {}: {}", script.display(), e);
            std::process::exit(1);
        }
    }

    // Step 2: Update settings.json
    let settings_file = settings_path();
    let mut settings: Value = if settings_file.exists() {
        let content = match fs::read_to_string(&settings_file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ✗ cannot read {}: {}", settings_file.display(), e);
                std::process::exit(1);
            }
        };
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  ✗ {} contains invalid JSON: {}", settings_file.display(), e);
                eprintln!("    fix the file manually before running --setup");
                std::process::exit(1);
            }
        }
    } else {
        Value::Object(Default::default())
    };

    let obj = settings.as_object_mut().unwrap();

    // Check if statusLine is already configured
    let expected_cmd = script.display().to_string();
    if let Some(existing) = obj.get("statusLine") {
        if let Some(existing_obj) = existing.as_object() {
            if let Some(cmd) = existing_obj.get("command") {
                let cmd_str = cmd.as_str().unwrap_or("");
                if cmd_str != expected_cmd && !cmd_str.is_empty() {
                    eprintln!("  ⚠ statusLine already configured: {}", cmd_str);
                    eprintln!("    to override, remove the existing statusLine key from:");
                    eprintln!("    {}", settings_file.display());
                    std::process::exit(1);
                }
            }
        }
    }

    // Set statusLine config
    obj.insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": script.display().to_string()
        }),
    );

    match fs::write(&settings_file, serde_json::to_string_pretty(&settings).unwrap_or_default()) {
        Ok(_) => println!("  ✓ updated {}", settings_file.display()),
        Err(e) => {
            eprintln!("  ✗ failed to update {}: {}", settings_file.display(), e);
            std::process::exit(1);
        }
    }

    println!("\n  done! rate limit data will appear in abtop after the next Claude response.");
    println!("  restart any running Claude Code sessions to activate.");
}
