#!/bin/bash
# Downloads the classic "Headspace" Windows Media Player skin from the Internet
# Archive and makes it Applebum's skin. The skin isn't part of this repository:
# it isn't ours to redistribute, and no license for it is published.
#
# Any other .wmz or .wsz works the same way: drop it on Applebum's window.
set -euo pipefail

url="https://archive.org/download/windowsmediaplayerskinscollection/Headspace.wmz"
sha256="2d2788b25bff711feb1ba858596adae80683ab9f09a5e369a2cfd7ca7f49ae1d"
config="$HOME/Library/Application Support/xyz.vladik.applebum"

if pgrep -x Applebum >/dev/null; then
    echo "Quit Applebum first: it rewrites its settings when it closes." >&2
    exit 1
fi

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
curl -fsSL --max-time 120 -o "$scratch/Headspace.wmz" "$url"
actual="$(shasum -a 256 "$scratch/Headspace.wmz" | cut -d' ' -f1)"
if [ "$actual" != "$sha256" ]; then
    echo "The download isn't the file this script expects, so nothing was installed." >&2
    exit 1
fi

mkdir -p "$config/skins"
mv "$scratch/Headspace.wmz" "$config/skins/Headspace.wmz"
/usr/bin/python3 - "$config/settings.json" <<'PY'
import json, os, sys
path = sys.argv[1]
settings = json.load(open(path)) if os.path.exists(path) else {}
settings["skin"] = "Headspace.wmz"
settings["winamp_window"] = True
settings["random_skin"] = False
json.dump(settings, open(path, "w"), indent=2)
PY
echo "Headspace is on. Open Applebum."
