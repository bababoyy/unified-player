#!/usr/bin/env bash
# Local build entry point. Every command uses one feature set (FEATURES,
# default `ci`, the same set bacon uses) so all builds share one target/ tree
# instead of accumulating a copy of the dependency graph per feature set.
set -euo pipefail

cd "$(dirname "$0")/.."
features="${FEATURES-ci}"
feature_args=(-p unified-player --no-default-features --features "$features")
target_dir="${CARGO_TARGET_DIR:-target}"
# check_rust_profile.sh names the empty feature set `minimal`.
profile="${features:-minimal}"

usage() {
  cat <<EOF
Usage: scripts/dev.sh <command> [args...]

  build [args]    debug build
  run [args]      debug build and run; args go to unified-player
  check           cargo check (all targets)
  test            tests, via the CI profile script
  lint            clippy for the $profile and minimal profiles (CI baseline)
  verify          fmt check + test + lint, i.e. what CI runs
  release         optimized build
  size            show what is taking space in $target_dir
  trim            delete incremental caches (the part that grows fastest)
  clean           delete $target_dir entirely

Set FEATURES to use another feature set (e.g. FEATURES=standard, or
FEATURES= for none). test and lint accept only standard, ci or none.
EOF
}

command_name="${1:-}"
[[ $# -gt 0 ]] && shift
case "$command_name" in
build) cargo build "${feature_args[@]}" "$@" ;;
run) cargo run "${feature_args[@]}" -- "$@" ;;
check) cargo check --all-targets "${feature_args[@]}" "$@" ;;
test) scripts/check_rust_profile.sh test "$profile" ;;
lint)
  scripts/check_rust_profile.sh clippy "$profile"
  scripts/check_rust_profile.sh clippy minimal
  ;;
verify)
  cargo fmt --all -- --check
  "$0" test
  "$0" lint
  ;;
release) cargo build --release "${feature_args[@]}" "$@" ;;
size)
  du -sh "$target_dir" 2>/dev/null || { echo "$target_dir does not exist"; exit 0; }
  # One du per path: a single du skips directories already counted under a parent.
  for dir in "$target_dir"/*/ "$target_dir"/*/{deps,build,incremental}; do
    [[ -d "$dir" ]] && du -sh "$dir"
  done | sort -rh
  ;;
trim) rm -rf "$target_dir"/*/incremental && echo "Removed incremental caches" ;;
clean) cargo clean --target-dir "$target_dir" ;;
"" | -h | --help | help) usage ;;
*) echo "Unknown command: $command_name" >&2; usage >&2; exit 2 ;;
esac
