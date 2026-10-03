#!/usr/bin/env bash
# The same non-release commands are usable locally and in manual Actions runs.
set -euo pipefail

command_name="${1:-test}"
profile="${2:-standard}"
case "$command_name" in
test | clippy) ;;
*) echo "Expected test or clippy" >&2; exit 2 ;;
esac
features=(--no-default-features)
case "$profile" in
standard | ci) features+=(--features "$profile") ;;
minimal) ;;
*) echo "Expected standard, ci or minimal" >&2; exit 2 ;;
esac

args=("$command_name" --locked --target-dir "${CARGO_TARGET_DIR:-target}" "${features[@]}")
if [[ "$command_name" == clippy ]]; then
  # Existing API/layout debt baseline, shared by every selected profile.
  args+=(--
    -A clippy::redundant_closure_for_method_calls
    -A clippy::result_large_err
    -A clippy::large_enum_variant
    -A clippy::too_many_arguments
    -A clippy::type_complexity
    -A clippy::unnecessary_wraps
    -A clippy::large_futures
    -A clippy::duration_suboptimal_units
    -A clippy::unused_self
    -A clippy::needless_pass_by_value
    -A clippy::fn_params_excessive_bools
    -D warnings)
fi
cargo "${args[@]}"
