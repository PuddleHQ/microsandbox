#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# forward-cap.sh - more than 64 concurrent forwards on one ssh session (Linux, real microVM; the
# Linux twin of repros.ps1's forward-cap case).
#
# usage: forward-cap.sh <runtime dir> <work dir>
#   <runtime dir>  holds msb and libkrunfw.so.<version> (as msb looks for it beside itself)
#   <work dir>     private MSB_HOME, key, logs; created if missing
#
# Boots one python:3.12-alpine sandbox that runs hold-server.py (greets every connection, holds it),
# then through `ssh -L` over `msb ssh serve --stdio`:
#   plain: 200 held connections at once; all 200 must be greeted (stock: the 65th on is refused).
#   retry: 300 at once, the first 100 closed after 4 s; the last 100 must wait for a slot and be
#          served after the release, within the 10 s retry window (guard: something really queued).
# Prints "CASE <name>: PASS|FAIL ..." per phase. Exit 0: all passed. 1: a phase failed. 2: setup failed.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
rt=$(cd "${1:?runtime dir}" && pwd)
mkdir -p "${2:?work dir}"
work=$(cd "$2" && pwd)
name=${FORWARD_CAP_NAME:-fcap}
msb=$rt/msb
export MSB_HOME=$work/home
export MSB_PATH=$msb
port=${FORWARD_CAP_PORT:-18195}
mkdir -p "$work/guest"
cp "$here/hold-server.py" "$work/guest/"
command -v node >/dev/null || { echo "setup: node not found"; exit 2; }

ssh_pid=
# shellcheck disable=SC2329 # called by the EXIT trap
cleanup() {
  [ -n "$ssh_pid" ] && kill "$ssh_pid" 2>/dev/null
  "$msb" stop "$name" >/dev/null 2>&1
  "$msb" rm -f "$name" >/dev/null 2>&1
}
trap cleanup EXIT

echo "runtime: $("$msb" --version) ($msb)"
if ! "$msb" create --name "$name" --replace --cpus 2 --memory 1024 \
  --mount-dir "$work/guest:/opt/repro:ro" python:3.12-alpine >"$work/create.log" 2>&1; then
  echo "setup: msb create failed"; cat "$work/create.log"; exit 2
fi
"$msb" exec --no-tty --no-stdin "$name" -- sh -c \
  'nohup python3 /opt/repro/hold-server.py 18095 >/tmp/hold.log 2>&1 &' </dev/null >"$work/server.log" 2>&1 \
  || { echo "setup: guest server start failed"; cat "$work/server.log"; exit 2; }

ssh-keygen -q -t ed25519 -f "$work/id" -N '' -C forward-cap || exit 2
"$msb" ssh authorize --file "$work/id.pub" >"$work/authorize.log" 2>&1 \
  || { echo "setup: msb ssh authorize failed"; cat "$work/authorize.log"; exit 2; }
cat >"$work/ssh_config" <<CFG
Host $name
  User root
  ProxyCommand $msb ssh serve %n --stdio
  IdentityFile $work/id
  IdentitiesOnly yes
  IdentityAgent none
  BatchMode yes
  LogLevel ERROR
  StrictHostKeyChecking no
  UserKnownHostsFile $work/known_hosts
CFG

fail=0
rows=()
# phase <label> <port> <n> <release>
phase() {
  local label=$1 p=$2 n=$3 release=$4
  ssh -F "$work/ssh_config" -N -o ExitOnForwardFailure=yes -L "127.0.0.1:$p:127.0.0.1:18095" "$name" \
    >"$work/ssh-$label.log" 2>&1 &
  ssh_pid=$!
  for _ in $(seq 30); do
    kill -0 "$ssh_pid" 2>/dev/null || break
    (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null && break
    sleep 1
  done
  if ! kill -0 "$ssh_pid" 2>/dev/null; then
    echo "CASE $label: FAIL (precondition: ssh -L exited at start)"; cat "$work/ssh-$label.log"
    rows+=("| $label | FAIL | ssh -L exited at start |"); fail=1; ssh_pid=; return
  fi
  local out
  out=$(node "$here/cap-client.mjs" "$p" "$n" "$release" 4000 40000 2>&1)
  echo "  $label: $out"
  kill "$ssh_pid" 2>/dev/null; wait "$ssh_pid" 2>/dev/null; ssh_pid=
  local g late maxms verdict=
  g=$(sed -n 's/.* greeted=\([0-9]*\).*/\1/p' <<<"$out")
  maxms=$(sed -n 's/.*maxGreetMs=\([0-9]*\).*/\1/p' <<<"$out")
  late=$(sed -n 's/.*lateGreeted=\([0-9]*\).*/\1/p' <<<"$out")
  if [ -z "$g" ]; then verdict="no summary line from the client"
  elif [ "$g" -ne "$n" ]; then verdict="only $g of $n forwards served"
  elif [ "$release" -gt 0 ]; then
    if [ "${late:-0}" -lt 1 ] || [ "${maxms:-0}" -lt 3500 ]; then
      verdict="precondition: nothing was queued (late=${late:-?}, max greet ${maxms:-?} ms)"
    elif [ "$maxms" -gt 12000 ]; then verdict="queued forwards took $maxms ms (> 12 s)"
    fi
  fi
  if [ -n "$verdict" ]; then
    echo "CASE $label: FAIL ($verdict)"; rows+=("| $label | FAIL | $verdict |"); fail=1
  else
    echo "CASE $label: PASS ($g/$n, max ${maxms} ms, ${late} after the release)"
    rows+=("| $label | PASS | $g/$n served, max greet ${maxms} ms |")
  fi
}

phase plain "$port" 200 0
phase retry "$((port + 1))" 300 100

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  { echo "### forward-cap ($("$msb" --version))"; echo; echo "| phase | result | detail |"; echo "|---|---|---|"
    printf '%s\n' "${rows[@]}"; } >>"$GITHUB_STEP_SUMMARY"
fi
exit "$fail"
