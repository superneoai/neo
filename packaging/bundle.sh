#!/bin/sh
set -eu

required_version="0.11.8"
repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

actual_version=$(cargo packager --version 2>/dev/null || true)
if [ "$actual_version" != "cargo-packager $required_version" ]; then
    echo "cargo-packager $required_version is required; install it with:" >&2
    echo "  cargo install cargo-packager --version $required_version --locked" >&2
    exit 1
fi

cargo packager "$@"

plist="$repository_root/dist/NEO.app/Contents/Info.plist"
if [ -f "$plist" ]; then
    # cargo-packager 0.11.8 always adds this obsolete key.
    # Remove the key so the app has no Carbon declaration.
    plutil -remove LSRequiresCarbon "$plist"
fi
