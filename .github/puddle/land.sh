#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# land.sh - publish an auto-bump candidate whose gates passed (fork-only file).
#
# usage: land.sh <candidate dir> <version, e.g. 0.7.8-puddle.1> <upstream tag>
# Run in a full clone of TijsVK/microsandbox. <candidate dir> is the run's `puddle-candidate`
# artifact (gh run download <run id> -R TijsVK/microsandbox -n puddle-candidate -D <dir>).
#
# 1. libkrun / russh legs (if replayed): push branch puddle-<base version> (libkrun also gets tag
#    msb_krun-v<base version>-puddle.<n>). Never forced: an existing branch must already match.
# 2. msb: push the merge commit (new stack + old `puddle` as second parent) to puddle-bump/v<version>,
#    open the PR into `puddle`, then move `puddle` to it (a fast-forward, which marks the PR merged).
# 3. Tag v<version> on the new stack tip.
#
# Credentials: FORKS_TOKEN (fine-grained, Contents + Workflows + Pull requests write on the three
# forks) is used for every push of new commits when set, else the clone's own git credentials (a
# person's gh login). TAG_REMOTE (default origin) takes the tag push: in the workflow that is the
# GITHUB_TOKEN checkout, so the tag starts no other workflow and the caller publishes the release;
# from a person's clone the tag push starts puddle-release.yml, which builds, gates and publishes.
set -euo pipefail
d=$(cd "$1" && pwd) version=$2 up_tag=$3
tag="v$version"
py=$(command -v python3 || command -v python)
field() { "$py" -c 'import json,sys; c=json.load(open(sys.argv[1])); print(c.get(sys.argv[2], {}).get(sys.argv[3], ""))' "$d/candidate.json" "$1" "$2"; }
remote() { if [ -n "${FORKS_TOKEN:-}" ]; then echo "https://x-access-token:${FORKS_TOKEN}@github.com/$1"; else echo "https://github.com/$1"; fi; }
GIT_ID=(-c user.name="puddle auto-bump" -c user.email=puddle@tijsvankampen.be)

push_new_branch() { # <repo dir> <remote url> <branch> <sha>
  local have
  have=$(git -C "$1" ls-remote "$2" "refs/heads/$3" | cut -f1)
  if [ -n "$have" ]; then
    [ "$have" = "$4" ] || { echo "::error::$3 already exists at $have, want $4; not forcing"; exit 1; }
    echo "$3 already at $4"
  else
    git -C "$1" push -q "$2" "$4:refs/heads/$3"
    echo "pushed $3 = $4"
  fi
}

# Preflight: refuse before pushing anything when a branch exists with other commits.
check_branch() { # <remote url> <branch> <sha>
  local have
  have=$(git ls-remote "$1" "refs/heads/$2" | cut -f1)
  if [ -n "$have" ] && [ "$have" != "$3" ]; then echo "::error::$2 already exists at $have, want $3; not forcing"; exit 1; fi
}
for leg in libkrun russh; do
  [ -n "$(field "$leg" bundle)" ] || continue
  check_branch "$(field "$leg" url)" "$(field "$leg" branch)" "$(field "$leg" sha)"
done
check_branch "https://github.com/${GITHUB_REPOSITORY:-TijsVK/microsandbox}" "puddle-bump/$tag" "$(field msb merge)"

for leg in libkrun russh; do
  b=$(field "$leg" bundle)
  [ -n "$b" ] || continue
  url=$(field "$leg" url); branch=$(field "$leg" branch); sha=$(field "$leg" sha)
  base=$(field "$leg" base); base_url=$(field "$leg" base_url); ver=$(field "$leg" base_version)
  slug=${url#https://github.com/}
  m=$(mktemp -d)
  git init -q --bare "$m"
  git -C "$m" fetch -q --no-tags "$base_url" "$base"
  git -C "$m" fetch -q --no-tags "$d/$b" "refs/heads/$branch:refs/heads/$branch"
  push_new_branch "$m" "$(remote "$slug")" "$branch" "$sha"
  leg_tag=$(field "$leg" tag)
  git -C "$m" fetch -q --no-tags "$d/$b" "refs/tags/$leg_tag:refs/tags/$leg_tag"
  have=$(git ls-remote "$url" "refs/tags/$leg_tag^{}" "refs/tags/$leg_tag" | cut -f1 | tail -1)
  if [ -n "$have" ]; then
    [ "$have" = "$sha" ] || { echo "::error::tag $leg_tag already exists at $have, want $sha; not moving it"; exit 1; }
    echo "$leg_tag already at $sha"
  else
    git -C "$m" "${GIT_ID[@]}" tag -f -a "$leg_tag" -m "$leg_tag: $leg fork at $ver plus the puddle fixes (auto-bump for $up_tag)" "$sha"
    git -C "$m" push -q "$(remote "$slug")" "refs/tags/$leg_tag"
    echo "tagged $leg_tag"
  fi
done

sha=$(field msb sha); merge=$(field msb merge); old=$(field msb old)
git fetch -q --no-tags "https://github.com/${UPSTREAM_REPO:-superradcompany/microsandbox}" "refs/tags/$up_tag:refs/tags/$up_tag"
git fetch -q --no-tags "$d/msb.bundle" "+refs/heads/puddle-candidate:refs/puddle-bump/tip"
git fetch -q --no-tags "$d/msb-merge.bundle" "+refs/heads/puddle-candidate-merge:refs/puddle-bump/merge"
slug=${GITHUB_REPOSITORY:-TijsVK/microsandbox}
bump="puddle-bump/$tag"
push_new_branch . "$(remote "$slug")" "$bump" "$merge"

if [ -z "$(gh pr list -R "$slug" --head "$bump" --state all --json number --jq '.[0].number // empty')" ]; then
  { echo "Auto-bump of the puddle fork to upstream \`$up_tag\`: every gate passed (build, tests, boot smoke, repros, volume from the previous release)."
    echo; cat "$d/summary.md"; } > "$d/pr.md"
  gh pr create -R "$slug" --base puddle --head "$bump" --title "auto-bump: $up_tag -> $tag" --body-file "$d/pr.md"
fi

cur=$(git ls-remote "https://github.com/$slug" refs/heads/puddle | cut -f1)
if [ "$cur" != "$merge" ]; then
  [ "$cur" = "$old" ] || { echo "::error::puddle moved since the replay ($old -> $cur); rerun the auto-bump"; exit 1; }
  git push -q "$(remote "$slug")" "$merge:refs/heads/puddle"
  echo "puddle fast-forwarded to $merge"
fi

if git ls-remote --exit-code --tags "https://github.com/$slug" "refs/tags/$tag" >/dev/null; then
  echo "$tag already exists"
else
  git "${GIT_ID[@]}" tag -a "$tag" -m "$tag: microsandbox $up_tag plus the puddle patch stack (auto-bump)" "$sha"
  git push -q "${TAG_REMOTE:-origin}" "refs/tags/$tag"
  echo "tagged $tag = $sha"
fi
