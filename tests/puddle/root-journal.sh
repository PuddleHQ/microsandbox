#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# root-journal.sh - the protected root-disk journal (GHSA-3f94-vm6x-3jv3): the guest gets only
# sandboxes/<name>/guest-state/ as /.msb, the authoritative root-disk.json stays on the host, and a
# sandbox made by an older release migrates safely (Linux host, real microVMs).
#
# usage: root-journal.sh <runtime dir> <work dir> [<old runtime dir>]
#   <runtime dir>      holds msb and libkrunfw.so.<version> (as msb looks for it beside itself)
#   <work dir>         private MSB_HOME, logs; created if missing
#   <old runtime dir>  the same for a release from before the protection (v0.7.8); without it the
#                      two upgrade cases are skipped
#
# Two alpine sandboxes: a stopped victim with /victim-marker on its root disk, and the target with
# /own-marker on its own. The hostile journal names the victim's upper.ext4 as the target's only
# layer and keeps the target's own upper.ext4 as launch base.
# Cases:
#   hidden:        the guest's /.msb holds no root-disk.json, runtime-boot-id or checkpoint files,
#                  and the host keeps the journal in sandboxes/<name>/root-disk.json
#   restart:       stop and start; /own-marker is still there
#   guest-tamper:  guest root writes the hostile journal to /.msb/root-disk.json, then stop and start:
#                  the start succeeds, /own-marker is there, /victim-marker is not, the victim's image
#                  and the host journal are unchanged
#   upgrade-plain: a sandbox made and stopped by the old release starts on this one: its marker is
#                  there, the layout record and guest-state directory exist, and the journal is on
#                  the host side
#   upgrade-armed: the guest rewrote the journal under the old release; the first start on this one
#                  is refused ("migration/recovery required") and the victim's image is unchanged
# Writes by a host user are out of scope for the protection and are not tested.
# Prints "CASE <name>: PASS|FAIL ..." per case. Exit 0: all passed. 1: a case failed. 2: setup failed.
set -uo pipefail

rt=$(cd "${1:?runtime dir}" && pwd)
mkdir -p "${2:?work dir}"
work=$(cd "$2" && pwd)
old_rt=""
[ -z "${3:-}" ] || old_rt=$(cd "$3" && pwd)
name=${ROOT_JOURNAL_NAME:-rjournal}
victim=$name-victim
image=${ROOT_JOURNAL_IMAGE:-mirror.gcr.io/library/alpine}
msb=$rt/msb
export MSB_HOME=$work/home
export MSB_PATH=$msb

use() { # runtime dir: run the following commands with that release's msb
  msb=$1/msb
  export MSB_PATH=$msb
}

# shellcheck disable=SC2329 # called by the EXIT trap
cleanup() {
  use "$rt"
  for home in "$work/home" "$work/home-up"; do
    for sandbox in "$name" "$victim" "$name-plain" "$name-armed" "$name-avictim"; do
      MSB_HOME=$home "$msb" stop "$sandbox" >/dev/null 2>&1
      MSB_HOME=$home "$msb" rm -f "$sandbox" >/dev/null 2>&1
    done
  done
}
trap cleanup EXIT

fail=0
case_result() { # name ok detail
  if [ "$2" = 1 ]; then echo "CASE $1: PASS $3"; else echo "CASE $1: FAIL $3"; fail=1; fi
}
g() { # sandbox command
  "$msb" exec --no-tty --no-stdin "$1" -- sh -c "$2" </dev/null
}
start() { # sandbox log
  timeout 300 "$msb" start "$1" >"$2" 2>&1
}
create() { # sandbox
  "$msb" create --name "$1" --replace --cpus 1 --memory 512 "$image" >"$work/create-$1.log" 2>&1 \
    || { echo "setup: msb create $1 failed"; cat "$work/create-$1.log"; exit 2; }
}
hex() { od -An -tx1 -N16 /dev/urandom | tr -d ' \n'; }
hostile_for() { # own upper.ext4, victim upper.ext4
  printf '{"schema":"microsandbox.runtime-root-disk/1","volume_id":"vol_%s","device_id":"vdb","layout":"managed-upper","published_generation":0,"launch_base":"%s","layers":[{"layer_id":"layer_%s","path":"%s","format":"raw","integrity_root":null}]}' \
    "$(hex)" "$1" "$(hex)" "$2"
}
sum_of() { sha256sum "$1" | cut -d' ' -f1; }
verdict() { [ "$1" = "$2" ] && echo unchanged || echo CHANGED; }

echo "runtime: $("$msb" --version) ($msb)"
[ -z "$old_rt" ] || echo "old runtime: $("$old_rt/msb" --version) ($old_rt/msb)"

create "$victim"
g "$victim" "echo victim > /victim-marker && sync" || { echo "setup: victim write failed"; exit 2; }
"$msb" stop "$victim" >/dev/null 2>&1 || { echo "setup: msb stop $victim failed"; exit 2; }
victim_img=$MSB_HOME/sandboxes/$victim/upper.ext4
[ -f "$victim_img" ] || { echo "setup: no victim image at $victim_img"; exit 2; }
victim_sum=$(sum_of "$victim_img")

create "$name"
g "$name" "echo own > /own-marker && sync" || { echo "setup: own write failed"; exit 2; }
seen=$(g "$name" "ls -a /.msb | tr '\n' ' '" 2>&1)
echo "guest /.msb: $seen"
journal=$MSB_HOME/sandboxes/$name/root-disk.json
own_img=$MSB_HOME/sandboxes/$name/upper.ext4
ok=0
if g "$name" "! test -e /.msb/root-disk.json && ! test -e /.msb/runtime-boot-id && ! ls /.msb | grep -qi checkpoint" \
  && [ -f "$journal" ]; then ok=1; fi
case_result hidden "$ok" "ls -a /.msb: $seen; host journal $([ -f "$journal" ] && echo present || echo MISSING)"

"$msb" stop "$name" >/dev/null 2>&1 || { echo "setup: msb stop $name failed"; exit 2; }
[ -f "$journal" ] || { echo "setup: no journal at $journal"; ls -la "$MSB_HOME/sandboxes/$name"; exit 2; }
cp "$journal" "$work/genuine.json"
echo "genuine journal: $(cat "$journal")"
hostile=$(hostile_for "$own_img" "$victim_img")

start "$name" "$work/start-restart.log"; rc=$?
marker=$(g "$name" "cat /own-marker" 2>&1 | tr -d '\r\n')
ok=0; [ "$rc" -eq 0 ] && [ "$marker" = own ] && ok=1
case_result restart "$ok" "start rc=$rc /own-marker=$marker"

g "$name" "printf '%s' '$hostile' > /.msb/root-disk.json && sync" \
  || { echo "setup: guest journal write failed"; exit 2; }
"$msb" stop "$name" >/dev/null 2>&1 || { echo "setup: msb stop $name failed"; exit 2; }
start "$name" "$work/start-guest-tamper.log"; rc=$?
marker=$(g "$name" "cat /own-marker 2>/dev/null; test -e /victim-marker && echo +victim" 2>&1 | tr -d '\r\n')
sum=$(sum_of "$victim_img")
same=changed; cmp -s "$journal" "$work/genuine.json" && same=unchanged
ok=0; [ "$rc" -eq 0 ] && [ "$marker" = own ] && [ "$sum" = "$victim_sum" ] && [ "$same" = unchanged ] && ok=1
case_result guest-tamper "$ok" "start rc=$rc markers=$marker victim image $(verdict "$sum" "$victim_sum") host journal $same"
"$msb" stop "$name" >/dev/null 2>&1

if [ -z "$old_rt" ]; then
  echo "CASE upgrade-plain: SKIP no old runtime"
  echo "CASE upgrade-armed: SKIP no old runtime"
  exit "$fail"
fi

# Sandboxes made by the old release, started by this one. A separate home: the catalog this release
# opened is newer than the old one understands.
export MSB_HOME=$work/home-up
use "$old_rt"
plain=$name-plain
armed=$name-armed
avictim=$name-avictim
create "$avictim"
g "$avictim" "echo victim > /victim-marker && sync" || { echo "setup: old victim write failed"; exit 2; }
"$msb" stop "$avictim" >/dev/null 2>&1 || { echo "setup: old stop $avictim failed"; exit 2; }
avictim_img=$MSB_HOME/sandboxes/$avictim/upper.ext4
[ -f "$avictim_img" ] || { echo "setup: no old victim image at $avictim_img"; exit 2; }
avictim_sum=$(sum_of "$avictim_img")

create "$plain"
g "$plain" "echo own > /own-marker && sync" || { echo "setup: old plain write failed"; exit 2; }
"$msb" stop "$plain" >/dev/null 2>&1 || { echo "setup: old stop $plain failed"; exit 2; }

create "$armed"
g "$armed" "echo own > /own-marker && sync" || { echo "setup: old armed write failed"; exit 2; }
hostile=$(hostile_for "$MSB_HOME/sandboxes/$armed/upper.ext4" "$avictim_img")
g "$armed" "printf '%s' '$hostile' > /.msb/root-disk.json && sync" \
  || { echo "setup: old guest journal write failed"; exit 2; }
"$msb" stop "$armed" >/dev/null 2>&1 || { echo "setup: old stop $armed failed"; exit 2; }

use "$rt"
start "$plain" "$work/start-upgrade-plain.log"; rc=$?
marker=$(g "$plain" "cat /own-marker" 2>&1 | tr -d '\r\n')
dir=$MSB_HOME/sandboxes/$plain
layout=missing; [ -f "$dir/state-layout.json" ] && layout=present
gstate=missing; [ -d "$dir/guest-state" ] && gstate=present
hostj=missing; [ -f "$dir/root-disk.json" ] && hostj=present
guard=missing; grep -qs 'upgrade-required' "$dir/runtime/root-disk.json" && guard=present
ok=0
[ "$rc" -eq 0 ] && [ "$marker" = own ] && [ "$layout" = present ] && [ "$gstate" = present ] \
  && [ "$hostj" = present ] && ok=1
case_result upgrade-plain "$ok" "start rc=$rc /own-marker=$marker state-layout.json $layout guest-state $gstate host journal $hostj old-path guard $guard"
sed 's/^/start (upgrade-plain): /' "$work/start-upgrade-plain.log" | tail -5
"$msb" stop "$plain" >/dev/null 2>&1

start "$armed" "$work/start-upgrade-armed.log"; rc=$?
named=0
grep -qs "migration/recovery required" "$work/start-upgrade-armed.log" && named=1
sum=$(sum_of "$avictim_img")
ok=0; [ "$rc" -ne 0 ] && [ "$named" = 1 ] && [ "$sum" = "$avictim_sum" ] && ok=1
case_result upgrade-armed "$ok" "start rc=$rc refusal named=$named victim image $(verdict "$sum" "$avictim_sum")"
sed 's/^/start (upgrade-armed): /' "$work/start-upgrade-armed.log" | tail -5
exit "$fail"
