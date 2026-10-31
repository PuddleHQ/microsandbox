#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# bootstrap-root.sh - the host folder behind a sandbox's bootstrap root (msb-bootstrap-root-*) is
# removed when the sandbox stops, when it fails to start, and, after a SIGKILL, by the next start
# (Linux, real microVM).
#
# usage: bootstrap-root.sh <runtime dir> <work dir>
#   <runtime dir>  holds msb and libkrunfw.so.<version> (as msb looks for it beside itself)
#   <work dir>     private MSB_HOME and TMPDIR (the folders land in a fresh directory below it, so
#                  a stray folder of another job cannot make a case pass or fail); created if missing
#
# Cases:
#   running:  while the sandbox runs, exactly one folder exists, and some process holds it open
#   stopped:  after `msb stop` no folder is left (red when the runtime's exit hook is removed)
#   failed:   a start that fails after the folder was made (the root disk image is gone) leaves none
#   killed:   SIGKILL of the runner leaves its folder (the case would otherwise prove nothing)
#   swept:    once the folder is older than the sweep's minimum age, the next start removes it, and
#             the new sandbox's own folder is untouched
# Prints "CASE <name>: PASS|FAIL ..." per case. Exit 0: all passed. 1: a case failed. 2: setup failed.
set -uo pipefail

rt=$(cd "${1:?runtime dir}" && pwd)
mkdir -p "${2:?work dir}"
work=$(cd "$2" && pwd)
name=${BOOTSTRAP_ROOT_NAME:-broot}
msb=$rt/msb
export MSB_HOME=$work/home
export MSB_PATH=$msb
TMPDIR=$(mktemp -d "$work/tmp.XXXXXX")
export TMPDIR
min_age=${BOOTSTRAP_ROOT_MIN_AGE:-30}

# shellcheck disable=SC2329 # called by the EXIT trap
cleanup() {
  "$msb" stop "$name" >/dev/null 2>&1
  "$msb" rm -f "$name" >/dev/null 2>&1
}
trap cleanup EXIT

fail=0
case_result() { # name ok detail
  if [ "$2" = 1 ]; then echo "CASE $1: PASS $3"; else echo "CASE $1: FAIL $3"; fail=1; fi
}
folders() { find "$TMPDIR" -maxdepth 1 -name 'msb-bootstrap-root-*' | sort; }
count() { folders | wc -l | tr -d ' '; }
# Wait up to $1 seconds for the folder count to reach $2.
wait_count() {

  for _ in $(seq "$1"); do [ "$(count)" = "$2" ] && return 0; sleep 1; done
  [ "$(count)" = "$2" ]
}
holders() { # pids that hold the folder $1 open
  find /proc/[0-9]*/fd -lname "$1" 2>/dev/null | cut -d/ -f3 | sort -u
}
create() {
  "$msb" create --name "$name" --replace --cpus 2 --memory 512 alpine >"$work/create.log" 2>&1
}

echo "runtime: $("$msb" --version) ($msb)"
create || { echo "setup: msb create failed"; cat "$work/create.log"; exit 2; }

# running
n=$(count)
folder=$(folders | head -1)
pids=$([ -n "$folder" ] && holders "$folder")
ok=0; [ "$n" = 1 ] && [ -n "$pids" ] && ok=1
case_result running "$ok" "folders=$n holder_pids=$(echo "$pids" | tr '\n' ' ')"

# stopped
"$msb" stop "$name" >"$work/stop.log" 2>&1
wait_count 20 0; n=$(count)
ok=0; [ "$n" = 0 ] && ok=1
case_result stopped "$ok" "folders left after stop: $n $(folders | tr '\n' ' ')"

# failed: the root disk image of the stopped sandbox is gone, so the start fails after the
# folder was made.
img=$MSB_HOME/sandboxes/$name/upper.ext4
[ -f "$img" ] || { echo "setup: no root disk image at $img"; exit 2; }
mv "$img" "$img.away"
"$msb" start "$name" >"$work/start-fail.log" 2>&1; start_rc=$?
wait_count 20 0; n=$(count)
mv "$img.away" "$img"
ok=0; [ "$start_rc" -ne 0 ] && [ "$n" = 0 ] && ok=1
echo "failed start: rc=$start_rc: $(tail -3 "$work/start-fail.log" | tr '\n' ' ')"
case_result failed "$ok" "start rc=$start_rc folders left: $n $(folders | tr '\n' ' ')"

# killed
"$msb" start "$name" >"$work/start.log" 2>&1 || { echo "setup: msb start failed"; cat "$work/start.log"; exit 2; }
folder=$(folders | head -1)
pids=$([ -n "$folder" ] && holders "$folder")
[ -n "$pids" ] || { echo "setup: no process holds $folder"; exit 2; }
# shellcheck disable=SC2086 # one pid per word
kill -9 $pids
sleep 2
n=$(count)
ok=0; [ "$n" = 1 ] && [ "$(folders | head -1)" = "$folder" ] && [ -z "$(holders "$folder")" ] && ok=1
case_result killed "$ok" "folders after SIGKILL of $(echo "$pids" | tr '\n' ' '): $n"
"$msb" stop "$name" >/dev/null 2>&1

# swept: wait out the minimum age, then start again.
sleep $((min_age + 2))
create || { echo "setup: second msb create failed"; cat "$work/create.log"; exit 2; }
new=$(folders | head -1)
ok=0; [ "$(count)" = 1 ] && [ -n "$new" ] && [ "$new" != "$folder" ] && [ ! -e "$folder" ] && ok=1
case_result swept "$ok" "old=$folder new=$new count=$(count)"
exit "$fail"
