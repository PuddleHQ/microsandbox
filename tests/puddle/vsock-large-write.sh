#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# vsock-large-write.sh - guest-to-host writes of 4 KiB to 128 KiB per call on a Unix-socket vsock
# route arrive intact (Linux host, real microVM).
#
# usage: vsock-large-write.sh <runtime dir> <work dir> [<MiB per chunk size>]
#   <runtime dir>  holds msb and libkrunfw.so.<version> (as msb looks for it beside itself)
#   <work dir>     private MSB_HOME, the route's socket, logs; created if missing
#
# Boots one python:3.12-alpine sandbox (no network) with --vsock <sock>:5000 to a host server
# (vsock-large-write.py host). The guest sends <MiB> (default 4) on one connection per chunk size,
# in send() calls of 4, 16, 32, 64 and 128 KiB, and compares the sha256 the host computed over
# what it received with its own. A guest kernel that splits a large packet's data over several
# descriptors (Linux 6.12.y does above about 32 KiB) needs the device to gather all of them.
# Exit 0: every chunk size arrived intact. 1: some did not. 2: setup failed (no verdict).
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
rt=$(cd "${1:?runtime dir}" && pwd)
mkdir -p "${2:?work dir}"
work=$(cd "$2" && pwd)
mib=${3:-4}
name=${VSOCK_LARGE_WRITE_NAME:-vsock-large-write}
msb=$rt/msb
export MSB_HOME=$work/home
sock=$work/route/w.sock
mkdir -p "$work/route" "$work/guest"
[ ${#sock} -lt 100 ] || { echo "setup: socket path too long for AF_UNIX: $sock"; exit 2; }
cp "$here/vsock-large-write.py" "$work/guest/"

host_pid=
# shellcheck disable=SC2329 # called by the EXIT trap
cleanup() {
  "$msb" stop "$name" >/dev/null 2>&1
  "$msb" rm "$name" >/dev/null 2>&1
  [ -n "$host_pid" ] && kill "$host_pid" 2>/dev/null
}
trap cleanup EXIT

echo "runtime: $("$msb" --version) ($msb)"
# The server prints "ready" once it listens; reading that line is the wait.
exec 3< <(exec python3 -u "$work/guest/vsock-large-write.py" host "$sock" 2>"$work/host.err")
host_pid=$!
read -r -t 30 ready <&3
[ "${ready:-}" = ready ] || { echo "setup: host server did not start"; cat "$work/host.err"; exit 2; }

if ! "$msb" create --name "$name" --replace --cpus 2 --memory 1024 --no-net \
  --vsock "$sock:5000" --mount-dir "$work/guest:/opt/t:ro" python:3.12-alpine \
  >"$work/create.log" 2>&1; then
  echo "setup: msb create failed"; cat "$work/create.log"; exit 2
fi

timeout 600 "$msb" exec --no-tty --no-stdin "$name" -- python3 /opt/t/vsock-large-write.py guest \
  $((mib << 20)) 4096 16384 32768 65536 131072 </dev/null >"$work/guest.out" 2>&1
code=$?
cat "$work/guest.out"
fail=0
for chunk in 4096 16384 32768 65536 131072; do
  line=$(grep "^WRITE {\"chunk\": $chunk," "$work/guest.out")
  if [[ $line == *'"ok": true'* ]]; then
    echo "CASE chunk=$chunk: PASS"
  else
    echo "CASE chunk=$chunk: FAIL"; fail=1
  fi
done
[ $code -eq 0 ] || [ $fail -eq 1 ] || { echo "guest exit $code"; fail=1; }
exit "$fail"
