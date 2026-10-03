# Binary Packaging Rehearsal

Binary distribution is being prepared. There are no published project binaries
or working public installer URLs yet. The README's source installation path
remains the available installation method.

## Planned Packages

The configuration pins cargo-dist 0.33.0 and selects only the `unified-player`
application for archive distribution. Cargo registry publication remains
disabled for every workspace package.

| Platform | Target | Archive |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |

These are build targets, not claims of live playback acceptance. Each candidate
uses `--no-default-features --features ci`: the `standard` rodio backend,
media controls and embedded YouTube QuickJS solver, plus image rendering,
notifications and fuzzy search. Daemon, alternate audio backends and private
capture are excluded. See [build profiles](build-features.md).

Archives include the application executable, project license, README, support
matrix and vendored EJS third-party notice. cargo-dist generates per-archive
SHA-256 checksums and shell/PowerShell installer scripts. Homebrew, AUR and
crates.io are later work.

## Review the Plan

With the pinned cargo-dist tool installed, this command plans the package
outputs without building the application:

```sh
dist plan
```

The selected first independent prerelease version is `0.1.0-alpha.1`; the
package version supplies the preview tag `v0.1.0-alpha.1`. A plan does not
create that tag or a release. The intended repository is
`bababoyy/unified-player`; access and hosting must be checked before publication.

The [cargo-dist configuration reference](https://axodotdev.github.io/cargo-dist/book/reference/config.html)
describes archive inputs, build features and installers. We use a hand-written
manual rehearsal workflow, so the config has no generated release CI.

## Run a Rehearsal

An operator must explicitly request a release build before running the
rehearsal. Once the reviewed workflow is committed and pushed to the intended
repository, open **Actions → Binary packaging rehearsal → Run workflow**.
Select the candidate branch, enter its exact 40-character commit SHA, and
choose a platform. Linux is the default; `all` starts four native build runners.
The input SHA must match the branch commit resolved by GitHub for that run.

The workflow checks out that commit, verifies the pinned packaging tool's
checksum, validates bundled assets and tracked publication paths, and builds
the selected native archive with a linkage report. The Linux job also generates
the installer scripts for review. Output files are uploaded as Actions artifacts
with seven-day retention. The workflow has `contents: read` and no release,
tag, package-registry or container publication step.

Installer scripts refer to future GitHub release assets. Do not run them as an
installation test until those reviewed assets exist; first unpack each Actions
archive directly in a clean environment.

## Check the Candidate

For each candidate, verify the archive checksum and included files, then run
the extracted executable's `--version`, `diagnostics` and offline demo. Review
the linkage report on Linux/macOS and document runtime libraries needed on a
clean machine. The Linux build uses Ubuntu 22.04; a build on that runner alone
does not establish a minimum glibc version or distribution compatibility.

A local Linux Mint 22.3 candidate passed checksum, extracted `--version`,
`--help` and isolated static diagnostics checks on 2026-10-04. It imports
glibc 2.39 symbols and dynamically links ALSA, D-Bus and OpenSSL 3. This host
check does not qualify the Ubuntu 22.04 runner archive or a clean installation.

Keep a short live check separate: Spotify playback, YouTube playback, provider
switching and clean exit. Record its commit, compiled features, platform and
result in the support matrix. Audible and terminal acceptance require a human
operator or the repository-required computer-use capability. A passing archive
build does not supply that evidence.

Before publication, settle the repository URL and prerelease version, review
dependency/license notices for the compiled graph, and verify installation
from each archive. Publish the reviewed artifacts only after explicit owner
authorization. Then test the hosted installers and replace the README's source
only installation wording with their real URLs.
