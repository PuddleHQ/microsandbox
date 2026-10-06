#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# publish-release.sh - create the fork release for a v<release>-puddle.N tag (fork-only file).
#
# usage: publish-release.sh <tag> <upstream repo> <upstream tag> <run url>
#        <dir with msb-windows-x86_64.exe, msb-linux-x86_64 + the fork-built agentd-x86_64>
#        <dir with upstream libkrunfw-windows-x86_64.dll and libkrunfw-linux-x86_64.so> [extra notes file]
# Needs GH_TOKEN with contents: write and GITHUB_REPOSITORY. Never overwrites an existing release.
set -euo pipefail
tag=$1 up_repo=$2 up_tag=$3 run_url=$4 msb_dir=$5 up_dir=$6 extra=${7:-}
if gh release view "$tag" -R "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
  echo "::error::release $tag already exists; not overwriting its assets"
  exit 1
fi
a=$(mktemp -d)
cp "$msb_dir/msb-windows-x86_64.exe" "$msb_dir/msb-linux-x86_64" "$msb_dir/agentd-x86_64" "$up_dir/libkrunfw-windows-x86_64.dll" "$up_dir/libkrunfw-linux-x86_64.so" "$a/"
(cd "$a" && sha256sum msb-windows-x86_64.exe libkrunfw-windows-x86_64.dll agentd-x86_64 msb-linux-x86_64 libkrunfw-linux-x86_64.so > checksums.sha256 && cat checksums.sha256)
{
  echo "puddle fork of microsandbox at \`$tag\`: upstream [\`$up_tag\`](https://github.com/$up_repo/releases/tag/$up_tag) plus the puddle patch stack (see the commits on top of \`$up_tag\`)."
  echo
  echo "- \`msb-windows-x86_64.exe\`: built by [this run]($run_url) with MSVC, static CRT, features \`embed-binaries,net,ssh\`, embedding the \`agentd-x86_64\` below."
  echo "- \`agentd-x86_64\`: the guest agent (static musl), built by the same run from the same source as the \`msb\` above. It is not upstream's: the per-client forward limit is raised in both."
  echo "- \`libkrunfw-windows-x86_64.dll\`: unchanged from upstream \`$up_tag\`, checked against its \`checksums.sha256\`."
  echo "- \`msb-linux-x86_64\`: built by the same run on ubuntu-22.04, features \`embed-binaries,net,ssh\`, embedding the \`agentd-x86_64\` above. Install it beside \`libkrunfw.so.<LIBKRUNFW_VERSION>\` (the next file, renamed)."
  echo "- \`libkrunfw-linux-x86_64.so\`: unchanged from upstream \`$up_tag\`, checked against its \`checksums.sha256\`."
  echo "- \`checksums.sha256\`: SHA-256 of the files above."
  if [ -n "$extra" ] && [ -f "$extra" ]; then echo; cat "$extra"; fi
  echo
  echo "Not an official microsandbox release."
} > "$a/notes.md"
gh release create "$tag" -R "$GITHUB_REPOSITORY" --verify-tag --title "$tag" --notes-file "$a/notes.md" \
  "$a/msb-windows-x86_64.exe" "$a/libkrunfw-windows-x86_64.dll" "$a/agentd-x86_64" "$a/msb-linux-x86_64" "$a/libkrunfw-linux-x86_64.so" "$a/checksums.sha256"
