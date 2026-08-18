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

app="$repository_root/dist/NEO.app"
if [ ! -d "$app" ]; then
    echo "cargo-packager did not create $app" >&2
    exit 1
fi

plist="$app/Contents/Info.plist"
# cargo-packager 0.11.8 adds this obsolete key.
plutil -remove LSRequiresCarbon "$plist"
# Sign release bundles after this plist change.

license="$app/Contents/Resources/Legal/AGPL-3.0-or-later.txt"
if [ ! -f "$license" ]; then
    echo "missing bundled license: $license" >&2
    exit 1
fi
if ! cmp -s LICENSE "$license"; then
    echo "bundled license differs from LICENSE: $license" >&2
    exit 1
fi
