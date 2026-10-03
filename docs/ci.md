# CI Controls

Pushes and pull requests do not start application builds or tests. Compilation,
platform compatibility checks, Nix packaging and live-provider tests require an
explicit manual run. No workflow publishes binaries or Docker images.

`Binary packaging rehearsal` is a separate manual-only candidate build. It
uploads archives to Actions artifacts and generates installers for review;
it does not create a release. See [binary packaging](binary-distribution.md)
for its exact-commit input, platform selection and remaining installation checks.

## Automatic Checks

`CI` runs one Ubuntu `fast-checks` job on each branch push. It checks Rust
formatting (without compilation), terminal ownership, bundled EJS hashes,
tracked publication paths/sizes, license copies, Docker exclusions, dependency
exception dates, workflow syntax, spelling, dependency use and changed-history
secrets. Its small Python regression tests exercise the CI gates themselves.
Same-repository PR events skip this job because the branch push already checks
the commit; fork PRs run it. A failed quick check fails the job. Pure documentation
pushes also run these inexpensive checks, never a Rust test matrix.

`Supply Chain Policy` runs on manifest/lock/license/policy changes and weekly on
Monday at 05:17 UTC. It checks advisories, licenses, dependency sources and the
Cargo package file lists without compiling the player. A manual run additionally
scans full history; ordinary pushes use only the CI secret scan. Weekly runs use
the default branch, so changes on another branch do not update the scheduled
policy until integrated and pushed to `main`.

Dependency exceptions in `deny.toml` must have an owner-accepted `until` or
`through` ISO date at the beginning of their reason. The date is inclusive in
UTC; expired or undated exceptions fail. Public RUSTSEC IDs from structured
cargo-deny notes appear in failure output; arbitrary raw notes and paths do not.
An accepted exception still exists even when the policy passes.

## Manual Builds And Tests

After this workflow version reaches the remote default branch, open **Actions →
CI → Run workflow**, select the branch to check, and choose:

| Input | Default | Effect |
| --- | --- | --- |
| `os` | `windows-latest` | Select Windows, Ubuntu, macOS, or `all` (three runners). |
| `profile` | `standard` | Select the daily-driver `standard`, broader `ci`, `minimal`, or `all` (three feature sets per OS). |
| `clippy` | false | Also run strict Clippy with the existing documented API/layout allowances. |

Choosing both `all` options starts nine Rust jobs. When all profiles are selected,
optional EJS checks run only once per operating system in its standard-profile
job. Each selected profile gets a separate target directory. The quick job must
pass before Rust runners start; each Rust job also checks its own checkout's EJS
bytes before compiling. Rust jobs have a 45-minute limit and matrix fail-fast.
A newer run of the same event/ref cancels the older one; a push does not cancel a
manual run. No release binary is built by this workflow.

Equivalent CLI invocation (only run when you intend to spend runner time):

```sh
gh workflow run ci.yml --repo bababoyy/unified-player --ref main \
  -f os=windows-latest -f profile=standard -f clippy=false
```

The UI/manual dispatch configuration must first exist on the default branch.
Local commits and a local merge alone do not update GitHub. The branch selector
can then target a development branch containing compatible workflow inputs.

## Cache Behavior

Manual Rust jobs use SHA-pinned `actions/cache` v5 restore/save actions for Cargo
downloads and compiled `target/ci` output. Incremental compilation is disabled to
reduce stored output. Keys include OS, architecture, toolchain, profile, optional
EJS selection and lockfile hash. No per-commit key is used, limiting cache churn
for repeated pushes with the same dependency graph. Cache saving can run after a
test/Clippy failure but never after cancellation; it creates only a missing key.
Caches are immutable: a partial cache from an unsuccessful initial run can be
removed through Actions cache management if a fresh populated cache is needed.
Cache availability is an optimization, not a reason to skip any check.

## Other Manual Workflows

- **Nix packaging (manual)** retains `nix build .` for an explicit packaging
  checkpoint, with concurrency cancellation and a 45-minute limit. The last
  automated Nix attempt failed downloading a crate with HTTP 403; changing its
  trigger does not establish Nix build success.
- **Provider Contract Smoke** retains the explicitly confirmed public YouTube
  resolver test. The unprovisioned authenticated self-hosted browser job was
  removed. Its ignored Rust tests remain available for local manual use.
- The duplicate CD and Docker publication workflows were removed. Their packaging
  source files remain historical/optional inputs; distribution needs a separate
  implementation and validation goal.

## Local Preflight And Known Limits

Use `actionlint` 1.7.12 with ShellCheck on PATH, plus:

```sh
python3 scripts/test_ci_checks.py
python3 scripts/check_ejs_assets.py
python3 scripts/check_publication_surface.py --tracked-only
python3 scripts/check_dependency_policy.py --self-test
python3 scripts/check_dependency_policy.py --check-exceptions
```

Python 3.11+ includes TOML support. Python 3.10 needs `tomli==2.2.1` installed.
The same profile runner is available locally; choose a target location with
sufficient disk space:

```sh
CARGO_TARGET_DIR=/path/to/build-cache bash scripts/check_rust_profile.sh test standard
CARGO_TARGET_DIR=/path/to/build-cache bash scripts/check_rust_profile.sh test minimal
CARGO_TARGET_DIR=/path/to/build-cache bash scripts/check_rust_profile.sh clippy standard
```

The September 10 maintenance pass found 120 existing standard-profile Clippy
errors after reproducing the steps that previous CI runs never reached. Clippy
remains strict and opt-in; its debt is not fixed or silently allowed by this
workflow change. Minimal compilation also has existing dead-code warnings.
A passing push check does not claim that manual Rust tests, Clippy, real audio,
terminal interaction, Windows execution or distribution packaging passed.
