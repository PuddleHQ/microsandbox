#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# pin-pr.sh - open the pin-bump PR in the puddle product repo for a new fork tag (fork-only file).
#
# usage: PIN_TOKEN=... pin-pr.sh <new version, e.g. 0.7.8-puddle.1> <release url>
# PIN_TOKEN: fine-grained token for TijsVK/puddle only (Contents + Pull requests read/write), secret
# PUDDLE_PIN_TOKEN (puddle D-60). Changes on a branch off `develop`: BUILT_FOR in
# crates/puddle-runtime/src/version.rs, and every `v<old>` / `=<old>` fork pin in the root Cargo.toml
# (Cargo.lock refreshed), and the new tag's pin lines in ci/msb-runtime.sha256 (the release's
# checksums.sha256, the tag's source commit, the firmware version carried from the old tag), so the
# Linux VM jobs fetch the prebuilt msb instead of building it. The PR says to run ci/windows-e2e.ps1 on the laptop before merging: adopting
# a fork tag stays a deliberate step.
set -euo pipefail
new=$1 release_url=$2
repo=TijsVK/puddle
w=$(mktemp -d)
git clone -q --depth 1 --branch develop "https://x-access-token:${PIN_TOKEN}@github.com/$repo.git" "$w/puddle"
cd "$w/puddle"
f=crates/puddle-runtime/src/version.rs
old=$(sed -n 's/^pub const BUILT_FOR: &str = "\(.*\)";$/\1/p' "$f")
[ -n "$old" ] || { echo "::error::no BUILT_FOR in $f"; exit 1; }
if [ "$old" = "$new" ]; then echo "puddle already pins $new"; exit 0; fi
branch="fork-pin/v$new"
if git ls-remote --exit-code origin "refs/heads/$branch" >/dev/null; then echo "branch $branch exists; not touching it"; exit 0; fi
git checkout -q -b "$branch"
sed -i "s/^pub const BUILT_FOR: &str = \"$old\";$/pub const BUILT_FOR: \&str = \"$new\";/" "$f"
sed -i -e "s/\"v$old\"/\"v$new\"/g" -e "s/\"=$old\"/\"=$new\"/g" Cargo.toml
if ! git diff --quiet -- Cargo.toml; then cargo metadata --format-version 1 >/dev/null; fi
# ci/msb-runtime.sha256 lists a tag newest first; ci/fetch-msb.sh refuses a tag with no line there.
pins=ci/msb-runtime.sha256
if [ -f "$pins" ] && ! grep -q "^v$new " "$pins"; then
  sums=$(curl -fsSL "${release_url/\/tag\//\/download\/}/checksums.sha256") || { echo "::error::no checksums.sha256 in $release_url"; exit 1; }
  src=$(curl -fsSL -H 'Accept: application/vnd.github.sha' "https://api.github.com/repos/TijsVK/microsandbox/commits/v$new")
  fw=$(sed -n "s/^v$old libkrunfw-linux-version //p" "$pins")
  [ -n "$src" ] && [ -n "$fw" ] || { echo "::error::no source commit or firmware version for v$new"; exit 1; }
  { printf '%s\n' "$sums" | awk -v t="v$new" 'NF == 2 { print t, $2, $1 }'
    echo "v$new source-commit $src"
    echo "v$new libkrunfw-linux-version $fw"; } >"$w/pins.new"
  first=$(grep -n "^v$old " "$pins" | head -1 | cut -d: -f1)
  [ -n "$first" ] || { echo "::error::no v$old lines in $pins"; exit 1; }
  { head -n $((first - 1)) "$pins"; cat "$w/pins.new"; tail -n +"$first" "$pins"; } >"$w/pins.out"
  cat "$w/pins.out" >"$pins"
fi
git -c user.name="puddle auto-bump" -c user.email=puddle@tijsvankampen.be commit -q -am "build: bundle msb fork $new (was $old)

Moves BUILT_FOR and the fork pins in Cargo.toml to v$new: $release_url.
Opened by the msb fork's auto-bump job."
git push -q origin "$branch"
GH_TOKEN=$PIN_TOKEN gh pr create -R "$repo" --base develop --head "$branch" \
  --title "build: bundle msb fork $new" \
  --body "The msb fork published [v$new]($release_url) (all fork gates green). This moves BUILT_FOR and the fork pins from $old.

Before merging: CI green, then run \`ci/windows-e2e.ps1\` on the laptop (W7: before every msb bump), and \`cargo xtask runtime --tag v$new\`.

Opened by TijsVK/microsandbox's puddle-autobump workflow."
