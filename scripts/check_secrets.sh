#!/usr/bin/env bash

set -euo pipefail

version="3.96.0"
case "$(uname -s)" in
Linux*)
  archive="trufflehog_${version}_linux_amd64.tar.gz"
  checksum="7105f1cd6577f058a9e39d0578f1a99c8a1e481e4d3512cd8a09acfe22a0fdc0"
  executable_name="trufflehog"
  python_command="python3"
  ;;
MINGW* | MSYS* | CYGWIN*)
  archive="trufflehog_${version}_windows_amd64.tar.gz"
  checksum="fbf918c52a1f29be96344e1c4696fe019cfc34fb1184fab31cf3e8347917b43a"
  executable_name="trufflehog.exe"
  python_command="python"
  ;;
Darwin*)
  case "$(uname -m)" in
  arm64)
    archive="trufflehog_${version}_darwin_arm64.tar.gz"
    checksum="87478306b95ca2420cfb844b7582383ac60b922e262350a0088e797f328d2e62"
    ;;
  x86_64)
    archive="trufflehog_${version}_darwin_amd64.tar.gz"
    checksum="a30d8f1095e031a81a668e1582f2ed479c3b50476cef86317e0fb74210c33617"
    ;;
  *)
    echo "Secret scan tool is unsupported on this macOS architecture."
    exit 1
    ;;
  esac
  executable_name="trufflehog"
  python_command="python3"
  ;;
*)
  echo "Secret scan tool is supported on Linux, Windows, and macOS."
  exit 1
  ;;
esac
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

curl -sSfL \
  "https://github.com/trufflesecurity/trufflehog/releases/download/v${version}/${archive}" \
  -o "${workdir}/${archive}"
echo "${checksum}  ${workdir}/${archive}" | sha256sum --check --strict >/dev/null
tar -xzf "${workdir}/${archive}" -C "$workdir" "$executable_name"
chmod +x "${workdir}/${executable_name}"

"$python_command" scripts/check_secret_results.py --self-test

base_sha="${BASE_SHA:-}"
head_sha="${HEAD_SHA:-HEAD}"
if [[ "$base_sha" == "0000000000000000000000000000000000000000" ]]; then
  base_sha=""
fi

args=(git file://. --branch "$head_sha")
if [[ -n "$base_sha" ]]; then
  args+=(--since-commit "$base_sha")
fi

if ! summary=$(
  set -o pipefail
  "${workdir}/${executable_name}" "${args[@]}" \
    --no-verification \
    --results=unverified,unknown \
    --filter-unverified \
    --filter-entropy=3.5 \
    --json \
    --log-level=-1 \
    --fail-on-scan-errors \
    2>/dev/null |
    "$python_command" scripts/check_secret_results.py
); then
  echo "Secret scan failed before producing a safe summary."
  exit 1
fi

printf '%s\n' "$summary"
if grep -qx "status=findings" <<<"$summary"; then
  exit 1
fi
if ! grep -qx "status=clean" <<<"$summary"; then
  echo "Secret scan summary did not report a recognized status."
  exit 1
fi
