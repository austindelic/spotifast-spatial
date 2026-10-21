#!/bin/sh
# Use the regular app bundle so Core Audio can identify cargo-run playback.
set -eu
binary=$1
shift
case "$binary" in
    */deps/*|*/examples/*) exec "$binary" "$@" ;;
esac
if [ "$(basename "$binary")" != spotifast ]; then
    exec "$binary" "$@"
fi
case "${1:-}" in
    --help|-h|--version|-V) exec "$binary" "$@" ;;
esac
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$here/../../Cargo.toml" | head -n 1)
app="$(dirname "$binary")/Spotifast-dev.app"
CODESIGN_IDENTITY= bash "$here/bundle.sh" "$binary" "$app" "$version" >&2
# Executing Contents/MacOS/Spotifast directly still registers an anonymous
# process. Launch Services is required for per-app spatial audio preferences.
exec open -W -n "$app" --args "$@"
