#!/usr/bin/env bash

set -euo pipefail

version="0.20.2"
case "$(uname -s)" in
Linux*)
  archive="cargo-deny-${version}-x86_64-unknown-linux-musl.tar.gz"
  checksum="9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f"
  executable_name="cargo-deny"
  python_command="python3"
  ;;
MINGW* | MSYS* | CYGWIN*)
  archive="cargo-deny-${version}-x86_64-pc-windows-msvc.tar.gz"
  checksum="975a22143262fd27476d19ee00c7af67978426e40e1dee94eed6bbade1cf87dc"
  executable_name="cargo-deny.exe"
  python_command="python"
  ;;
Darwin*)
  case "$(uname -m)" in
  arm64)
    archive="cargo-deny-${version}-aarch64-apple-darwin.tar.gz"
    checksum="fe67d82a10d8597a3549364cb733a3f9cc1bfff9031b7ae46384a9f2a72090c3"
    ;;
  x86_64)
    archive="cargo-deny-${version}-x86_64-apple-darwin.tar.gz"
    checksum="248da7f581724e470071990c088ffc55c811981715f4cbdb258621fb79f8b7a6"
    ;;
  *)
    echo "Dependency policy tool is unsupported on this macOS architecture."
    exit 1
    ;;
  esac
  executable_name="cargo-deny"
  python_command="python3"
  ;;
*)
  echo "Dependency policy tool is supported on Linux, Windows, and macOS."
  exit 1
  ;;
esac
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

curl -sSfL \
  "https://github.com/EmbarkStudios/cargo-deny/releases/download/${version}/${archive}" \
  -o "${workdir}/${archive}"
echo "${checksum}  ${workdir}/${archive}" | sha256sum --check --strict >/dev/null
tar -xf "${workdir}/${archive}" -C "$workdir"
cargo_deny="$(find "$workdir" -type f -name "$executable_name" -print -quit)"
if [[ -z "$cargo_deny" ]]; then
  echo "Dependency policy tool archive did not contain the expected executable."
  exit 1
fi
chmod +x "$cargo_deny"

"$python_command" scripts/check_dependency_policy.py --self-test
CARGO_DENY="$cargo_deny" "$python_command" scripts/check_dependency_policy.py
