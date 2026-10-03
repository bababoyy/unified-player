# Contributing

Thanks for helping improve `unified-player`. The project is being prepared for
public contribution in small, reviewable slices. Spotify support is the stable
provider boundary; YouTube Music support is provider-sensitive and should keep
its experimental limitations visible in code, tests, and documentation.

## Before You Start

- Check [`docs/support-matrix.md`](docs/support-matrix.md) before extending a
  provider feature. Do not imply parity where the provider API does not offer
  it.
- For a substantial change, open an issue or describe the proposed slice in a
  draft pull request before committing to a new subsystem.

## Local Checks

Use the fastest checks that match the change while iterating:

```console
cargo fmt --all --check
scripts/dev.sh check
cargo test -p unified-player --no-default-features --features ci path::to::focused_tests
```

Stick to the `ci` feature set that `scripts/dev.sh` and bacon use; each other
feature set compiles its own copy of the dependency graph into `target/`.
`scripts/dev.sh size` and `scripts/dev.sh trim` help when it grows.

At a milestone or before handoff, run the package suite and the relevant
feature check:

```console
scripts/dev.sh verify
cargo check -p unified-player --features private-capture
python scripts/check-ui-terminal-ownership.py
```

## Architecture Expectations

- Reuse Search and shared render helpers for new lists and tables.
- Keep interaction state in the page/state layer; renderers should project
  state rather than recreate selection, scrolling, or focus.
- Keep provider modules typed and provider-focused. They must not print to the
  terminal directly; diagnostics and user-facing status go through the
  centralized UI surfaces.
- Add a regression test for new selection, scrolling, overflow, resizing, or
  empty/loading/error behavior.
- Treat configuration as a user-facing API: document defaults, labels,
  descriptions, restart behavior, and focused parsing coverage.

## Out of Scope

Changes that do any of the following will not be accepted:

- download, save, or export audio or video for use outside the player;
- remove Spotify ads or unlock Premium features on a free account;
- bypass DRM, or decrypt Spotify audio outside librespot's normal playback;
- inflate play counts or automate listening.

## Sensitive Data

Never attach tokens, cookies, browser profiles, HAR files, signed media URLs,
raw provider responses, or unredacted logs. Use the application's bounded,
redacted diagnostics and support-bundle flows when they are available. When a
test needs provider data, use a fixture with synthetic identifiers.

## Pull Requests

Keep a pull request focused on one user goal. Explain the user-visible change,
provider limitations, configuration changes, and the checks you ran. Call out
manual verification that remains pending instead of implying it was covered by
unit tests.
