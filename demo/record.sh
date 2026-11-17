#!/bin/sh
# Records the Inbox tour (demo/inbox.py) and renders it to a GIF and an MP4.
# Needs asciinema 3, agg and ffmpeg; set AGG or FFMPEG to use other binaries.
#
#   demo/record.sh [out-dir]    # default: target/demo
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/target/demo}
agg=${AGG:-agg}
ffmpeg=${FFMPEG:-ffmpeg}
mkdir -p "$out"

exe=$root/target/release/werkbank
[ -x "$exe" ] || { echo "build first: cargo build --release" >&2; exit 1; }

asciinema rec --headless --overwrite --window-size 110x34 \
    --command "python3 '$root/demo/inbox.py' '$exe'" \
    "$out/inbox.cast" >/dev/null

# Apple Symbols draws ⏸ as text; the default emoji fallback would draw it double-width.
"$agg" --font-size 16 --font-family "JetBrains Mono,Fira Code,Menlo,Apple Symbols" \
    --last-frame-duration 3 "$out/inbox.cast" "$out/inbox.gif" >/dev/null 2>&1

"$ffmpeg" -loglevel error -y -i "$out/inbox.gif" \
    -movflags +faststart -pix_fmt yuv420p -vf "scale=trunc(iw/2)*2:trunc(ih/2)*2" \
    -c:v libx264 -crf 20 "$out/inbox.mp4"

ls -lh "$out/inbox.cast" "$out/inbox.gif" "$out/inbox.mp4"
