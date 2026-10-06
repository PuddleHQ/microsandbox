#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# use-candidate.sh - check out an auto-bump candidate that exists only as bundles (fork-only file).
#
# usage: use-candidate.sh <dir with candidate.json and bundles>
# Run in the repo root, checked out at the upstream release the candidate was replayed on. Fetches
# msb.bundle and checks out the new stack tip. For a libkrun/russh leg that was replayed too, it
# builds a local mirror holding the new branch and points `https://github.com/TijsVK/<repo>` at it
# (git url.insteadOf, cargo through the git CLI), so `cargo build --locked` gets exactly the commits
# the new Cargo.lock pins although nothing was pushed. Works in Git Bash on Windows too.
set -euo pipefail
d=$(cd "$1" && pwd)
py=$(command -v python3 || command -v python)
field() { "$py" -c 'import json,sys; c=json.load(open(sys.argv[1])); print(c.get(sys.argv[2], {}).get(sys.argv[3], ""))' "$d/candidate.json" "$1" "$2"; }
fileurl() { if (cd "$1" && pwd -W) >/dev/null 2>&1; then echo "file:///$(cd "$1" && pwd -W)"; else echo "file://$1"; fi; }

sha=$(field msb sha)
if ! git fetch -q --no-tags "$d/msb.bundle" "+refs/heads/puddle-candidate:refs/remotes/candidate/tip" 2>/dev/null; then
  echo "bundle prerequisites missing in a shallow checkout; unshallowing"
  git fetch -q --unshallow --no-tags origin || git fetch -q --no-tags origin
  git fetch -q --no-tags "$d/msb.bundle" "+refs/heads/puddle-candidate:refs/remotes/candidate/tip"
fi
git checkout -q --detach "$sha"
echo "candidate msb: $(git log -1 --format='%h %s')"

for leg in libkrun russh; do
  b=$(field "$leg" bundle)
  [ -n "$b" ] || continue
  url=$(field "$leg" url); branch=$(field "$leg" branch); tag=$(field "$leg" tag); base=$(field "$leg" base); base_url=$(field "$leg" base_url)
  m="${RUNNER_TEMP:-$d}/forks/$leg"
  mkdir -p "$m"
  git init -q --bare "$m"
  # Full history of the base: cargo's git fetch refuses a shallow source.
  git -C "$m" fetch -q --no-tags "$base_url" "$base"
  git -C "$m" fetch -q --no-tags "$d/$b" "+refs/heads/$branch:refs/heads/$branch" "+refs/tags/$tag:refs/tags/$tag"
  git config --global url."$(fileurl "$m")".insteadOf "$url"
  echo "candidate $leg: $url tag $tag = $(git -C "$m" rev-parse "refs/tags/$tag^{commit}") (local mirror $m)"
done
if [ -n "${GITHUB_ENV:-}" ]; then echo "CARGO_NET_GIT_FETCH_WITH_CLI=true" >> "$GITHUB_ENV"; fi
