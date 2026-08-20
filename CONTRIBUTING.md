# Contributing

Contributors retain copyright in their contributions.

By intentionally submitting a contribution for inclusion in NEO, you agree to
license it under `AGPL-3.0-or-later`. Contributions use the same license as the
project.

NEO requires no contributor license agreement, Developer Certificate of
Origin, copyright assignment, or sign-off.

## Release architecture scope

The macOS release pipeline builds the host architecture only. It does not
accept a `--target` option. It requires a thin 64-bit Mach-O executable and
rejects universal binaries. Add explicit target-artifact tracking and
per-slice identity checks before extending the pipeline to cross-compiled or
universal releases.
