#!/usr/bin/env bash
# Check a downloaded file against an expected SHA-256 digest; exits non-zero on mismatch.
# Works on the Linux, macOS and Windows (Git Bash) CI runners.
#
# Usage: scripts/verify_sha256.sh <file> <sha256-hex>
set -euo pipefail

file="${1:?usage: verify_sha256.sh <file> <sha256>}"
expected="${2:?usage: verify_sha256.sh <file> <sha256>}"

if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$file" | cut -d' ' -f1)"
else
    actual="$(shasum -a 256 "$file" | cut -d' ' -f1)"
fi

if [[ "$actual" != "$expected" ]]; then
    echo "SHA-256 mismatch for $file: expected $expected, got $actual" >&2
    exit 1
fi
echo "SHA-256 OK: $file"
