#!/bin/sh
set -eu

required_version="0.11.8"
required_about_version="0.8.2"
repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

actual_version=$(cargo packager --version 2>/dev/null || true)
if [ "$actual_version" != "cargo-packager $required_version" ]; then
    echo "cargo-packager $required_version is required; install it with:" >&2
    echo "  cargo install cargo-packager --version $required_version --locked" >&2
    exit 1
fi

actual_about_version=$(cargo about --version 2>/dev/null || true)
if [ "$actual_about_version" != "cargo-about $required_about_version" ]; then
    echo "cargo-about $required_about_version is required; install it with:" >&2
    echo "  cargo install cargo-about --version $required_about_version --locked" >&2
    exit 1
fi

# Regenerate the third-party notices so the bundle always matches this lockfile.
# --fail stops the release when a dependency license cannot be determined.
notices="packaging/generated/THIRD-PARTY-NOTICES.md"
mkdir -p "$(dirname "$notices")"
cargo about generate \
    --locked \
    --fail \
    -c packaging/licenses/about.toml \
    packaging/licenses/notices.md.hbs \
    -o "$notices"

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

expected_copyright="Copyright © 2026 ACTUAL LTD."
actual_copyright=$(plutil -extract NSHumanReadableCopyright raw "$plist" 2>/dev/null || true)
if [ "$actual_copyright" != "$expected_copyright" ]; then
    echo "unexpected bundle copyright: $actual_copyright" >&2
    exit 1
fi

license="$app/Contents/Resources/Legal/AGPL-3.0-or-later.txt"
if [ ! -f "$license" ]; then
    echo "missing bundled license: $license" >&2
    exit 1
fi
if ! cmp -s LICENSE "$license"; then
    echo "bundled license differs from LICENSE: $license" >&2
    exit 1
fi

bundled_notices="$app/Contents/Resources/Legal/THIRD-PARTY-NOTICES.md"
if [ ! -f "$bundled_notices" ]; then
    echo "missing bundled third-party notices: $bundled_notices" >&2
    exit 1
fi
if ! cmp -s "$notices" "$bundled_notices"; then
    echo "bundled notices differ from the generated notices: $bundled_notices" >&2
    exit 1
fi
# Reproduced license texts must stay verbatim, so reject HTML-escaped output.
if grep -q '&quot;\|&#x27;\|&amp;' "$bundled_notices"; then
    echo "bundled notices contain escaped entities: $bundled_notices" >&2
    exit 1
fi
