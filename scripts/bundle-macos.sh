#!/bin/sh
set -eu

profile="${1:-debug}"
case "$profile" in
    debug) binary="target/debug/audec" ;;
    release) binary="target/release/audec" ;;
    *) echo "usage: $0 [debug|release]" >&2; exit 2 ;;
esac

if [ ! -x "$binary" ]; then
    echo "$binary does not exist; build audec first" >&2
    exit 1
fi

bundle="target/Audec.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp resources/Info.plist "$bundle/Contents/Info.plist"
cp "$binary" "$bundle/Contents/MacOS/audec"

# Replacing either the executable or Info.plist invalidates any signature left
# by an earlier bundle. Refresh the local ad-hoc signature so launchd never
# rejects an incrementally rebuilt development app before `main` runs.
codesign --force --deep --sign - "$bundle"
codesign --verify --deep --strict "$bundle"

echo "$bundle"
