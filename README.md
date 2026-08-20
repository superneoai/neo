# NEO

NEO is an application by SUPERNEO, currently developed for macOS with GPUI and libneo.

## License

NEO uses the [`AGPL-3.0-or-later`](LICENSE) licence.

This repository holds the Corresponding Source for every NEO build:
<https://github.com/superneoai/neo>.

## Release

Install `cargo-packager` 0.11.8 and `cargo-about` 0.8.2, then run the release
stages independently:

```sh
cargo xtask package
cargo xtask sign
cargo xtask notarize
```

Packaging uses the release profile by default. For a faster local debug package,
run `cargo xtask package --debug`.

Signing uses the installed Developer ID Application identity or the
`NEO_SIGNING_IDENTITY` override. Notarization uses the `SUPERNEO_NOTARY`
Keychain profile.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md).

Copyright (c) 2026 ACTUAL LTD.
