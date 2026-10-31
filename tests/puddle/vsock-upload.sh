#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# vsock-upload.sh - guest-to-host throughput of one vsock connection to a host peer that reads.
# usage: vsock-upload.sh <runtime dir> <work dir> [<MiB> [<runs>]]   (Linux host, real microVM)
# Prints one "UPLOAD {...}" line per run. Used to check that a lower per-connection window keeps
# the throughput of a single agent session.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
rt=$(cd "${1:?runtime dir}" && pwd)
mkdir -p "${2:?work dir}"
work=$(cd "$2" && pwd)
mib=${3:-512}
runs=${4:-3}
name=${VSOCK_FLOOD_NAME:-vsock-upload}
msb=$rt/msb
export MSB_HOME=$work/home
sock=$work/route/sink.sock
mkdir -p "$work/route" "$work/guest"
cp "$here/vsock-flood.py" "$work/guest/"
sink_pid=
# shellcheck disable=SC2329 # called by the EXIT trap
cleanup() {
  "$msb" stop "$name" >/dev/null 2>&1
  "$msb" rm "$name" >/dev/null 2>&1
  [ -n "$sink_pid" ] && kill "$sink_pid" 2>/dev/null
}
trap cleanup EXIT
echo "runtime: $("$msb" --version)"
python3 "$work/guest/vsock-flood.py" sink "$sock" >"$work/sink.log" 2>&1 &
sink_pid=$!
for _ in $(seq 50); do [ -S "$sock" ] && break; sleep 0.1; done
[ -S "$sock" ] || { echo "setup: sink did not start"; exit 2; }
"$msb" create --name "$name" --replace --cpus 2 --memory 1024 --no-net \
  --vsock "$sock:5000" --mount-dir "$work/guest:/opt/t:ro" python:3.12-alpine \
  >"$work/create.log" 2>&1 || { echo "setup: msb create failed"; cat "$work/create.log"; exit 2; }
# A first, harmless command boots the VM so the runs do not include the boot.
"$msb" exec --no-tty --no-stdin "$name" -- true >/dev/null 2>&1 </dev/null
sleep 2
for _ in $(seq "$runs"); do
  "$msb" exec --no-tty --no-stdin "$name" -- python3 /opt/t/vsock-flood.py upload "$mib" chunk="${UPLOAD_CHUNK:-16384}" </dev/null 2>&1 | tail -5
done
