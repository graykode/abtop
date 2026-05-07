#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"
install_dir="${INSTALL_DIR:-$HOME/bin}"
install_path="${ABTOP_INSTALL_PATH:-$install_dir/abtop}"
target_dir="$repo_root/target"
built_binary="$target_dir/release/abtop"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found in PATH" >&2
  exit 1
fi

cargo build --release --locked --manifest-path "$repo_root/Cargo.toml" --target-dir "$target_dir"

if [[ ! -x "$built_binary" ]]; then
  echo "error: built binary missing: $built_binary" >&2
  exit 1
fi

mkdir -p -- "$(dirname -- "$install_path")"
tmp="$(mktemp "${install_path}.tmp.XXXXXX")"
trap 'rm -f -- "$tmp"' EXIT

cp -- "$built_binary" "$tmp"
chmod 0755 "$tmp"
mv -f -- "$tmp" "$install_path"
trap - EXIT

echo "installed $install_path"
"$install_path" --version
