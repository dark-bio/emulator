#!/usr/bin/env bash
# checksums.sh: SHA-256 verification, sourced by the firmware fetch to check a
# downloaded asset against the digest pinned in the workflow.
#
# macOS has no sha256sum by default; Linux and Git for Windows both do.
set -euo pipefail

sha256_of() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1"
  else
    shasum -a 256 "$1"
  fi | awk '{print $1}'
}

# verify_checksum <file> <expected hex digest>
verify_checksum() {
  local file="$1" expected="$2" actual
  actual="$(sha256_of "$file")"
  if [ "$expected" != "$actual" ]; then
    echo "checksum mismatch for $(basename "$file"): expected $expected, got $actual" >&2
    exit 1
  fi
}
