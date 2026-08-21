# Contributing

Contributors retain copyright in their contributions.

By intentionally submitting a contribution for inclusion in NEO, you agree to
license it under `AGPL-3.0-or-later`. Contributions use the same license as the
project.

NEO requires no contributor license agreement, Developer Certificate of
Origin, copyright assignment, or sign-off.

## Development

Run `just` to list development commands. Use `just run` for the pinned libneo
revision and `just ci` before pushing.

Set `NEO_LOCAL_LIBNEO_PATH` to the path of a libneo checkout before using
`just dev` or `just local`. Relative paths start at the NEO repository root. One
checkout path supplies the overrides for both `crates/libneo` and
`crates/libneo-gpui`:

```sh
NEO_LOCAL_LIBNEO_PATH=../libneo just local check
NEO_LOCAL_LIBNEO_PATH=../libneo just dev
```

## Release architecture scope

The macOS release pipeline builds the host architecture only. It does not
accept a `--target` option. It requires a thin 64-bit Mach-O executable and
rejects universal binaries. Add explicit target-artifact tracking and
per-slice identity checks before extending the pipeline to cross-compiled or
universal releases.
