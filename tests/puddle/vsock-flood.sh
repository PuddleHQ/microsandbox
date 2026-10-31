#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# vsock-flood.sh - cost of a guest that floods a vsock route (Linux host, real microVM).
#
# usage: [FLOOD_OPTS="k=v .."] vsock-flood.sh <runtime dir> <work dir> [<streams> [<hold s>]]
#   <runtime dir>  holds msb and libkrunfw.so.<version> (as msb looks for it beside itself)
#   <work dir>     private MSB_HOME, the route's socket, logs; created if missing
#
# Boots one python:3.12-alpine sandbox (no network, 1 GiB) with --vsock <sock>:5000 to a host server
# that never reads (vsock-flood.py host), then runs vsock-flood.py guest, which opens <streams>
# connections and writes on each until the route refuses more. While the guest holds the
# connections, the VMM process (`msb machine --name <name>`) is
# sampled every half second. Prints "FLOODRESULT" with the VMM's resident memory and descriptor
# count before and at the peak, and the peak of the enclosing cgroup when there is one (run it
# under `systemd-run --scope -p MemoryMax=...`).
# Exit 0: measured. 2: setup failed.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
rt=$(cd "${1:?runtime dir}" && pwd)
mkdir -p "${2:?work dir}"
work=$(cd "$2" && pwd)
streams=${3:-300}
hold=${4:-8}
name=${VSOCK_FLOOD_NAME:-vsock-flood}
msb=$rt/msb
export MSB_HOME=$work/home
sock=$work/route/flood.sock
mkdir -p "$work/route" "$work/guest"
[ ${#sock} -lt 100 ] || { echo "setup: socket path too long for AF_UNIX: $sock"; exit 2; }
cp "$here/vsock-flood.py" "$work/guest/"

host_pid=
# shellcheck disable=SC2329 # called by the EXIT trap
cleanup() {
  "$msb" stop "$name" >/dev/null 2>&1
  "$msb" rm "$name" >/dev/null 2>&1
  [ -n "$host_pid" ] && kill "$host_pid" 2>/dev/null
}
trap cleanup EXIT

echo "runtime: $("$msb" --version) ($msb)"
python3 "$work/guest/vsock-flood.py" host "$sock" >"$work/host.log" 2>&1 &
host_pid=$!
for _ in $(seq 50); do [ -S "$sock" ] && break; sleep 0.1; done
[ -S "$sock" ] || { echo "setup: host server did not start"; cat "$work/host.log"; exit 2; }

if ! "$msb" create --name "$name" --replace --cpus 2 --memory 1024 --no-net \
  --vsock "$sock:5000" --mount-dir "$work/guest:/opt/t:ro" python:3.12-alpine \
  >"$work/create.log" 2>&1; then
  echo "setup: msb create failed"; cat "$work/create.log"; exit 2
fi

# vmm_pid: the process named `msb machine --name <name>` with the largest resident set (a
# supervisor and the VMM share that name); empty when none.
vmm_pid() {
  local p r best="" max=-1
  for p in $(pgrep -f "msb machine --name $name "); do
    r=$(awk '/^VmRSS/ {print $2}' "/proc/$p/status" 2>/dev/null)
    if [ "${r:-0}" -gt "$max" ]; then max=${r:-0}; best=$p; fi
  done
  echo "$best"
}
rss_kib() { awk '/^VmRSS/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }
# queued_kib: bytes sitting unread in the route socket's accepted connections (the host's kernel buffers).
queued_kib() { ss -xn 2>/dev/null | awk -v s="$sock" '$5 == s {q += $3} END {print int(q / 1024)}'; }
fd_count() { find "/proc/$1/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l; }

# A first, harmless command boots the VM so the baseline includes the guest's own memory.
"$msb" exec --no-tty --no-stdin "$name" -- true >/dev/null 2>&1 </dev/null
sleep 2
pid=$(vmm_pid)
[ -n "$pid" ] || { echo "setup: no VMM process found for $name"; pgrep -af "$name" | head; exit 2; }
rss0=$(rss_kib "$pid"); fds0=$(fd_count "$pid")
echo "vmm pid $pid: rss ${rss0} KiB, ${fds0} descriptors, $(grep 'open files' "/proc/$pid/limits")"

# shellcheck disable=SC2086 # FLOOD_OPTS is a list of k=v words
"$msb" exec --no-tty --no-stdin "$name" -- \
  python3 /opt/t/vsock-flood.py guest "$streams" hold="$hold" ${FLOOD_OPTS:-} >"$work/guest.out" 2>&1 </dev/null &
guest_pid=$!
peak_rss=$rss0; peak_fds=$fds0; peak_q=0
while kill -0 "$guest_pid" 2>/dev/null; do
  pid=$(vmm_pid)
  if [ -n "$pid" ]; then
    r=$(rss_kib "$pid"); f=$(fd_count "$pid")
    [ "$r" -gt "$peak_rss" ] && peak_rss=$r
    [ "$f" -gt "$peak_fds" ] && peak_fds=$f
    q=$(queued_kib)
    [ "$q" -gt "$peak_q" ] && peak_q=$q
    [ -z "${VERBOSE:-}" ] || echo "sample: rss ${r} KiB, ${f} descriptors, ${q} KiB queued in the route's sockets"
  fi
  sleep 0.5
done
wait "$guest_pid"
guest_line=$(grep '^FLOOD ' "$work/guest.out" | tail -1)
[ -n "$guest_line" ] || { echo "setup: the guest printed no FLOOD line"; tail -5 "$work/guest.out"; exit 2; }
cg=$(sed -n 's|^0::||p' /proc/self/cgroup)
cgpeak=none
[ -r "/sys/fs/cgroup$cg/memory.peak" ] && cgpeak=$(( $(cat "/sys/fs/cgroup$cg/memory.peak") / 1048576 ))
result="FLOODRESULT streams=$streams rss_delta_mib=$(( (peak_rss - rss0) / 1024 )) fds_base=$fds0 fds_peak=$peak_fds cgroup_peak_mib=$cgpeak queued_kib=$peak_q host_queued_kib=$(awk '/^queued_bytes/ {if ($2 > m) m = $2} END {print int(m / 1024)}' "$work/host.log") host_conns=$(awk '/^queued_bytes/ {c = $4} END {print c}' "$work/host.log")"
echo "$guest_line"
echo "$result"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  { echo "### vsock flood, $("$msb" --version)"; echo; echo '```'; echo "$guest_line"; echo "$result"; echo '```'; } >>"$GITHUB_STEP_SUMMARY"
fi
