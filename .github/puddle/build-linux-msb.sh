#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# build-linux-msb.sh - build the Linux msb of the checked-out fork tag (fork-only file).
#
# usage: build-linux-msb.sh <agentd-x86_64> <upstream libkrunfw-linux-x86_64.so> <version> <out dir>
# Run from the tag's checkout. Builds with upstream's Linux steps (release-linux.yml "Build msb")
# and the fork's own lock, embedding the given guest agent, then checks what `msb` is and links.
# Leaves <out dir>/msb-linux-x86_64 and <out dir>/runtime/{msb,libkrunfw.so.<ver>}.
# Needs cargo and libcap-ng-dev.
set -euo pipefail
agentd=$(realpath "$1") krunfw=$(realpath "$2") version=$3
mkdir -p "$4"
out=$(realpath "$4")

fw=$(sed -n 's/^pub const LIBKRUNFW_VERSION: &str = "\(.*\)";$/\1/p' crates/utils/lib/lib.rs)
[ -n "$fw" ] || { echo "::error::LIBKRUNFW_VERSION not found in crates/utils/lib/lib.rs"; exit 1; }

embed=$(mktemp -d)
install -m 755 "$agentd" "$embed/agentd"
MSB_EMBED_ARTIFACTS_DIR=$embed cargo build --locked --release --no-default-features \
  --features embed-binaries,net,ssh -p microsandbox-cli
cp target/release/msb "$out/msb-linux-x86_64"

# The agentd inside msb is the one given, not a downloaded one.
want=$(sha256sum "$agentd" | cut -d' ' -f1)
staged=$(find target/release/build -path '*microsandbox-filesystem-*/out/agentd' -type f)
[ -n "$staged" ] || { echo "::error::no staged agentd under target/release/build"; exit 1; }
for f in $staged; do
  got=$(sha256sum "$f" | cut -d' ' -f1)
  echo "$f: $got"
  [ "$got" = "$want" ] || { echo "::error::embedded agentd differs from the given agentd ($want)"; exit 1; }
done

# The firmware sits beside msb under the name msb looks for (libkrunfw.so.<version>).
mkdir -p "$out/runtime"
install -m 755 "$out/msb-linux-x86_64" "$out/runtime/msb"
cp "$krunfw" "$out/runtime/libkrunfw.so.$fw"
got=$("$out/runtime/msb" --version)
echo "msb --version: $got (firmware $fw)"
[ "$got" = "msb $version" ] || { echo "::error::msb --version printed '$got', want 'msb $version'"; exit 1; }

# Only system libraries: the binary has to run on a plain runner or developer host.
ldd "$out/runtime/msb"
bad=$(ldd "$out/runtime/msb" | awk '{ print $1 }' | grep -v -E '^(linux-vdso|libc|libm|libgcc_s|libcap-ng|libdl|libpthread|librt|libutil|/lib64/ld-linux-x86-64)\.so' || true)
[ -z "$bad" ] || { echo "::error::unexpected shared libraries: $bad"; exit 1; }
python3 scripts/smoke/cli/runtime-discovery.py --msb "$out/runtime/msb" --libkrunfw "$out/runtime/libkrunfw.so.$fw"
