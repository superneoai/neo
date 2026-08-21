default:
    @just --list

# Launch a debug bundle built against the pinned libneo revision.
run:
    cargo xtask bundle --debug
    open dist/NEO.app

# Build and launch the app against sibling local checkouts.
dev:
    cargo xtask dev

# Run Cargo with local dependency overrides.
local *args:
    cargo xtask local {{args}}

# Run the checks enforced by CI.
ci:
    cargo fmt --all --check
    cargo xtask check-source
    cargo build --workspace --locked
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked
    cargo deny --locked check
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
    cargo xtask check-sdk
    cargo xtask bundle

# Build and verify the release app bundle.
bundle:
    cargo xtask bundle

# Sign the bundled app.
sign:
    cargo xtask sign

# Notarize the signed app bundle.
notarize:
    cargo xtask notarize
