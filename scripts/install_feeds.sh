#!/usr/bin/env bash
# Make the *installed* app self-contained: copy the generated feed files into the
# app's own data directory and point the configuration at that copy.
#
#   ./scripts/install_feeds.sh            # copy feeds, repoint feeds.toml
#   ./scripts/install_feeds.sh --dry-run  # show what would change
#
# Why: with `include` pointing into the working tree, /Applications/Artifeeds.app
# stops working the moment the project moves, and a project folder that happens
# to sit under Documents or Desktop costs a permission prompt on every launch.
# Copying the feeds next to the database removes the dependency entirely.
#
# The user's own `custom.toml` (feeds added in the app) is never overwritten.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE="$ROOT/config/blogs"
DATA="${ARTIFEEDS_DATA_DIR:-$HOME/Library/Application Support/Artifeeds}"
FEEDS="$DATA/feeds"
CONFIG="$DATA/feeds.toml"
DRY=0
[[ "${1:-}" == "--dry-run" ]] && DRY=1

if [[ ! -d "$SOURCE" ]]; then
  echo "no generated feeds at $SOURCE — run: python3 scripts/import_cib.py --out config" >&2
  exit 1
fi
if [[ ! -f "$CONFIG" ]]; then
  echo "no configuration at $CONFIG — start the app once to create it" >&2
  exit 1
fi

echo "from  $SOURCE"
echo "to    $FEEDS"

if [[ $DRY -eq 1 ]]; then
  for file in "$SOURCE"/*.toml; do
    echo "  copy $(basename "$file")"
  done
  echo "  repoint include in $CONFIG"
  exit 0
fi

mkdir -p "$FEEDS"
copied=0
for file in "$SOURCE"/*.toml; do
  name="$(basename "$file")"
  # `custom.toml` is the reader's own additions, not generated output.
  if [[ "$name" == "custom.toml" && -f "$FEEDS/$name" ]]; then
    continue
  fi
  cp "$file" "$FEEDS/$name"
  copied=$((copied + 1))
done
echo "copied $copied file(s)"

# Repoint the include, once, keeping a backup of the file we edit.
python3 - "$CONFIG" "$FEEDS" <<'PY'
import pathlib
import re
import sys

config = pathlib.Path(sys.argv[1])
target = sys.argv[2]
text = config.read_text()

if target in text:
    print("include already points at the data directory")
    raise SystemExit(0)

# `include` must appear before any [table]; replace the whole line in place so
# the file keeps its comments, settings and ordering.
new_line = f'include = ["{target}"]'
if re.search(r'^include\s*=.*$', text, flags=re.M):
    updated = re.sub(r'^include\s*=.*$', new_line, text, count=1, flags=re.M)
else:
    updated = new_line + "\n" + text

backup = config.with_suffix(".toml.bak")
if not backup.exists():
    backup.write_text(text)
config.write_text(updated)
print(f"include → {target}")
PY

echo "done — restart the app to pick the change up"
