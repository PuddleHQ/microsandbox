#!/usr/bin/env bash
# Check one downloaded release asset against the release's checksums.sha256.
# Usage: verify-asset.sh <checksums.sha256> <file> <asset name>
# Exits non-zero when the asset is not listed exactly once or its sha256 differs.
set -euo pipefail
sums=$1 file=$2 name=$3
expected=$(awk -v n="$name" '$2 == n || $2 == "*" n { print $1 }' "$sums")
count=$(printf '%s' "$expected" | grep -c . || true)
if [ "$count" -ne 1 ]; then
  echo "::error::$name is listed $count times in $(basename "$sums") (want exactly 1)"
  exit 1
fi
actual=$(sha256sum "$file" | awk '{ print $1 }')
if [ "$actual" != "$expected" ]; then
  echo "::error::sha256 mismatch for $name: got $actual, release lists $expected"
  exit 1
fi
echo "ok $name $actual"
