#!/bin/sh
set -eu

asset_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$asset_dir/.." && pwd)
cd "$repo_root"

if ! command -v vhs >/dev/null 2>&1; then
    echo "VHS 0.11.0 is required (macOS: brew install vhs)." >&2
    exit 1
fi

if ! command -v ffmpeg >/dev/null 2>&1; then
    echo "ffmpeg is required to extract the theme screenshots." >&2
    exit 1
fi

case "$(vhs --version)" in
    *0.11.0*) ;;
    *)
        echo "Expected VHS 0.11.0; found: $(vhs --version)" >&2
        exit 1
        ;;
esac

cargo build --release
vhs assets/demo.tape

for tape in assets/themes/*.tape; do
    recording=${tape%.tape}.preview.gif
    screenshot=${tape%.tape}.png
    if [ -e "$recording" ]; then
        unlink "$recording"
    fi
    if [ -e "$screenshot" ]; then
        unlink "$screenshot"
    fi
    vhs "$tape"
    ffmpeg -loglevel error -y -sseof -0.1 -i "$recording" -frames:v 1 "$screenshot"
    unlink "$recording"
done
