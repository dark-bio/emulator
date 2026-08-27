#!/usr/bin/env bash
# checksums.sh: SHA-256 helpers, shared between the firmware fetch, which
# verifies a published digest, and the release build, which generates one per
# asset. Both sides use a same-named .sha256 holding a `<hash>  <filename>`
# line, so a downloader can check it with `sha256sum -c`.
#
# macOS has no sha256sum by default; Linux and Git for Windows both do.
#
# Source it for the functions, or run it to write a .sha256 next to every file
# in a directory:
#   .github/scripts/checksums.sh <dir>
set -euo pipefail

sha256_line() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1"
  else
    shasum -a 256 "$1"
  fi
}

sha256_of() {
  sha256_line "$1" | awk '{print $1}'
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

# Run rather than sourced. Hashed from inside the directory so each file records
# its bare name, which is what a downloader checking it in place expects.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  cd "${1:?usage: checksums.sh <dir>}"
  # The glob expands once, so the .sha256 files written here are not hashed.
  for f in *; do
    [ -f "$f" ] || continue
    sha256_line "$f" > "$f.sha256"
  done
  ls -la
fi
