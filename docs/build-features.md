# Build Feature Profiles

Use one named profile for a build instead of mixing feature flags between
commands. Cargo features are additive, so an alternate backend must start from
`--no-default-features`.

All builds go through `scripts/dev.sh`, which applies the selected profile and
shares one `target/` directory between profiles. Set `FEATURES` to pick the
profile; it defaults to `ci`. Run `scripts/dev.sh help` for every command.

## CI And Broad UI

The `ci` profile is `standard` plus image rendering, notifications, and fuzzy
search. Use it for day-to-day work; bacon uses it too.

```sh
scripts/dev.sh check
scripts/dev.sh test
scripts/dev.sh lint
```

## Standard

The daily-driver profile, and Cargo's default: rodio playback, media
controls, and the embedded QuickJS solver for YouTube (`youtube-quickjs`).

```sh
FEATURES=standard scripts/dev.sh run
```

## Minimal And Alternate Backends

Use the minimal profile (no features) for provider/state work that does not
need integrated Spotify playback:

```sh
FEATURES= scripts/dev.sh test
```

Choose exactly one alternate backend; the script always starts from
`--no-default-features`:

```sh
FEATURES=pulseaudio-backend,media-control scripts/dev.sh check
```

Do not pass two backend features together. `private-capture` is a separate
opt-in lane. A build without `youtube-quickjs` (including the minimal profile)
solves YouTube player challenges with Node.js.

Every profile other than `ci` compiles its own copy of the affected
dependencies, so switch profiles only when the change needs it. Use
`scripts/dev.sh size` to see what `target/` holds and `scripts/dev.sh trim` to
drop incremental caches. Release builds remain opt-in.

## Workflow Lint Before Pushing

Run this inexpensive check from the repository root when changing workflows or
their lint configuration; it does not build the player or start GitHub Actions:

```sh
actionlint -version
shellcheck --version
actionlint
```

Use actionlint **1.7.12**, matching `.github/workflows/ci.yml`, and keep
ShellCheck on `PATH` so shell steps are checked too. The Ubuntu runner used for
the September 10, 2026 check had ShellCheck **0.9.0**. Installation instructions:
[actionlint](https://github.com/rhysd/actionlint/blob/v1.7.12/docs/install.md) and
[ShellCheck](https://github.com/koalaman/shellcheck#installing).

CI builds and tests are manual-only. Pushes and PRs run inexpensive source
checks; they do not compile the application. The obsolete CD/Docker workflows
and their actionlint exceptions have been removed. See [CI controls](ci.md) for
manual operating-system/profile selection, caching and known Clippy debt.

Python checks use Python 3.11 or newer. On Python 3.10, install `tomli==2.2.1`
for TOML parsing. `python scripts/check_ejs_assets.py` verifies bundled bytes
without compiling Rust; keep the corresponding `.gitattributes` LF rules.

## Optional Features

The prebuilt binaries use the `ci` profile, so they include cover art,
notifications, and fuzzy search. From a checkout, add features to
`cargo install --path unified-player --locked`.

### Image

`--features image` shows the current track's cover beside the playback
controls when the window has at least 32 rows and room for the track details.
[`ratatui-image`](https://github.com/benjajaja/ratatui-image) detects the
terminal's graphics protocol (Kitty, iTerm2, or Sixel) at startup and falls
back to block characters. The detection query goes through the terminal, so
inside a nested terminal (such as Neovim's) it reaches only the inner one and
falls back to block characters.

`--features pixelate` (implies `image`) draws a pixelated cover; set its
resolution with `cover_img_pixels` in [`app.toml`](config.md).

### Daemon

`--features daemon` adds `-d` / `--daemon`, which keeps playing with no window
open. It needs integrated playback (an audio backend) and is not supported on
Windows. On macOS it cannot run with media controls, which are on by default,
so build it without them:

```sh
cargo install --path unified-player --locked --no-default-features --features daemon,rodio-backend
```
