default:
    @just --list

# Launch a debug package built against the pinned libneo revision.
run:
    cargo xtask package --debug
    open dist/NEO.app

# Build and launch the app against the sibling libneo checkout.
local-run:
    cargo xtask local-cargo build --release --quiet
    cp target/release/neo dist/NEO.app/Contents/MacOS/neo
    codesign --force --sign - dist/NEO.app
    open dist/NEO.app

# Run Cargo against the sibling libneo checkout.
local-cargo *args:
    cargo xtask local-cargo {{args}}

# Run the checks enforced by CI.
gate:
    cargo fmt --all --check
    cargo xtask check-source
    cargo build --workspace --locked
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked
    cargo deny --locked check
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
    cargo xtask check-sdk
    cargo xtask package

# Build and verify the release app bundle.
package:
    cargo xtask package

# Sign the packaged app bundle.
sign:
    cargo xtask sign

# Notarize the signed app bundle.
notarize:
    cargo xtask notarize
