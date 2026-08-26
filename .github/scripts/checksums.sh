#!/usr/bin/env bash
# checksums.sh: SHA-256 helpers, shared between the firmware fetch, which
# verifies a published digest, and the release build, which generates one per
# asset. The convention on both sides is a same-named .sha256 file holding a
# `<hash>  <filename>` line, so a downloader can check it with `sha256sum -c`.
#
# macOS has no sha256sum by default (shasum -a 256 instead); Linux and Git for
# Windows' bundled coreutils both have it.
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

# Compares against the first whitespace-delimited field of $1.sha256 rather than
# feeding the file to `sha256sum -c`, so this doesn't depend on the checksum
# file's recorded filename matching our own local path.
verify_checksum() {
  local file="$1" recorded actual
  recorded="$(awk '{print $1}' "${file}.sha256")"
  actual="$(sha256_of "$file")"
  if [ "$recorded" != "$actual" ]; then
    echo "checksum mismatch for $(basename "$file"): expected $recorded, got $actual" >&2
    exit 1
  fi
}

# Run rather than sourced. Hashes are taken from inside the directory so each
# file records its own bare name, which is what `sha256sum -c` expects of
# someone checking a downloaded asset where it landed.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  cd "${1:?usage: checksums.sh <dir>}"
  # The glob expands once, before the loop body runs, so the .sha256 files
  # written here are not themselves hashed on a later iteration.
  for f in *; do
    [ -f "$f" ] || continue
    sha256_line "$f" > "$f.sha256"
  done
  ls -la
fi
