#!/usr/bin/env bash
# checksums.sh: SHA-256 verification, sourced by the firmware fetch to check a
# downloaded asset against the .sha256 published beside it.
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

# Compares against the first field of $1.sha256 rather than using `sha256sum -c`,
# so the recorded filename need not match our local path.
verify_checksum() {
  local file="$1" recorded actual
  recorded="$(awk '{print $1}' "${file}.sha256")"
  actual="$(sha256_of "$file")"
  if [ "$recorded" != "$actual" ]; then
    echo "checksum mismatch for $(basename "$file"): expected $recorded, got $actual" >&2
    exit 1
  fi
}
