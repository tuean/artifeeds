#!/usr/bin/env bash
# The one command to run after changing code: build, install into /Applications,
# refresh the feeds, and restart the installed app.
#
#   ./scripts/build.sh                # build + install + launch
#   ./scripts/build.sh --no-launch    # build + install only
#   ./scripts/build.sh --skip-feeds   # do not re-copy the generated feed files
#
# Extra arguments are passed to cargo, so `./scripts/build.sh -p artifeeds-app`
# builds one crate.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_ARGS=()
BUNDLE_ARGS=()
SKIP_FEEDS=0

for arg in "$@"; do
  case "$arg" in
    --no-launch)   BUNDLE_ARGS+=("--no-launch") ;;
    --skip-feeds)  SKIP_FEEDS=1 ;;
    *)             CARGO_ARGS+=("$arg") ;;
  esac
done

cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> cargo build --release ${CARGO_ARGS[*]:-}"
cargo build --release ${CARGO_ARGS[@]+"${CARGO_ARGS[@]}"}

if [[ $SKIP_FEEDS -eq 0 ]]; then
  echo "==> refresh feeds"
  "$ROOT/scripts/install_feeds.sh" | sed 's/^/    /'
fi

echo "==> bundle and install"
"$ROOT/scripts/make_app_bundle.sh" ${BUNDLE_ARGS[@]+"${BUNDLE_ARGS[@]}"}
