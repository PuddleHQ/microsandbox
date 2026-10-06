#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# linux-discard.sh - a guest `fstrim` on a disk volume returns the freed space to the host image
# (Linux host, real microVM, msb's default bounded writeback on).
#
# usage: linux-discard.sh <runtime dir> <work dir>
#   <runtime dir>  holds msb and libkrunfw.so.<version> (as msb looks for it beside itself)
#   <work dir>     private MSB_HOME, logs; created if missing
#
# Boots one debian:trixie-slim sandbox with a 2 GiB disk volume on /data, writes 1 GiB of random
# data, deletes it, runs `fstrim /data`, then stops the sandbox. After each step it prints the
# image's allocated size on the host. Cases:
#   advertised: the guest block device of /data reports discard (discard_max_bytes > 0)
#   fstrim:     `fstrim -v /data` exits 0
#   shrunk:     the image's allocated size after the trim (and after the stop) is under 300 MiB
#               (1 GiB written, ext4 metadata and journal stay; stock: refused, the image keeps ~1 GiB)
# Prints "CASE <name>: PASS|FAIL ..." per case. Exit 0: all passed. 1: a case failed. 2: setup failed.
set -uo pipefail

rt=$(cd "${1:?runtime dir}" && pwd)
mkdir -p "${2:?work dir}"
work=$(cd "$2" && pwd)
name=${LINUX_DISCARD_NAME:-ldisc}
vol=$name-vol
msb=$rt/msb
export MSB_HOME=$work/home
export MSB_PATH=$msb

# shellcheck disable=SC2329 # called by the EXIT trap
cleanup() {
  "$msb" stop "$name" >/dev/null 2>&1
  "$msb" rm -f "$name" >/dev/null 2>&1
  "$msb" volume rm "$vol" >/dev/null 2>&1
}
trap cleanup EXIT

mib() { echo $(( $(stat -c %b "$1") * 512 / 1048576 )); }
fail=0
case_result() { # name ok detail
  if [ "$2" = 1 ]; then echo "CASE $1: PASS $3"; else echo "CASE $1: FAIL $3"; fail=1; fi
}

echo "runtime: $("$msb" --version) ($msb)"
"$msb" volume create --kind disk --size 2G "$vol" >"$work/volume.log" 2>&1 \
  || { echo "setup: msb volume create failed"; cat "$work/volume.log"; exit 2; }
"$msb" create --name "$name" --replace --cpus 2 --memory 1024 \
  --mount-named "$vol:/data:kind=disk,size=2G" debian:trixie-slim >"$work/create.log" 2>&1 \
  || { echo "setup: msb create failed"; cat "$work/create.log"; exit 2; }
img=$MSB_HOME/volumes/$vol/disk.raw
[ -f "$img" ] || { echo "setup: no volume image at $img"; exit 2; }
g() { "$msb" exec --no-tty --no-stdin "$name" -- sh -c "$1" </dev/null; }

echo "image after create: $(mib "$img") MiB allocated"
dev=$(g "findmnt -no SOURCE /data" | tr -d '\r\n'); dev=${dev#/dev/}
adv=$(g "cat /sys/block/$dev/queue/discard_max_bytes" | tr -d '\r\n')
echo "guest device $dev: discard_max_bytes=$adv"
g "head -c 1073741824 /dev/urandom > /data/big && sync" || { echo "setup: guest write failed"; exit 2; }
written=$(mib "$img")
echo "image after 1 GiB written: $written MiB allocated"
g "rm /data/big && sync" || { echo "setup: guest delete failed"; exit 2; }
echo "image after delete: $(mib "$img") MiB allocated"
trim_out=$(g "fstrim -v /data" 2>&1); trim_rc=$?
echo "fstrim /data: rc=$trim_rc: $trim_out"
g "sync"
running=$(mib "$img")
echo "image after fstrim (running): $running MiB allocated"
g "fstrim -v /" 2>&1 | sed 's/^/fstrim \/ (info): /'
"$msb" stop "$name" >/dev/null 2>&1
stopped=$(mib "$img")
echo "image after stop: $stopped MiB allocated"

ok=0; [ "${adv:-0}" -gt 0 ] 2>/dev/null && ok=1
case_result advertised "$ok" "discard_max_bytes=$adv"
ok=0; [ "$trim_rc" -eq 0 ] && ok=1
case_result fstrim "$ok" "rc=$trim_rc"
ok=0; [ "$written" -gt 900 ] && [ "$running" -lt 300 ] && [ "$stopped" -lt 300 ] && ok=1
case_result shrunk "$ok" "written=${written}MiB trimmed=${running}MiB stopped=${stopped}MiB"
exit "$fail"
