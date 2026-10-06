#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# vsock-tail.sh - tail loss on a Unix-socket vsock route (Linux/macOS host, real microVM).
#
# usage: vsock-tail.sh <runtime dir> <work dir> [<cases file>]
#   <runtime dir>  holds msb and libkrunfw.so.<version> (as msb looks for it beside itself)
#   <work dir>     private MSB_HOME, the route's socket, results; created if missing
#   <cases file>   one case per line, "<case> <count> <size> [k=v ...]" (see vsock-tail.py);
#                  default: the list below
#
# Boots one python:3.12-alpine sandbox (no network) with --vsock <sock>:5000 to a host server
# (vsock-tail.py host), then runs each case as a guest client through msb exec. A case passes
# when every connection delivered every byte, in order, before EOF. Prints "CASE <spec>: PASS|LOSS
# ..." per case and a Markdown table (also to $GITHUB_STEP_SUMMARY when set).
# Exit 0: all cases passed. 1: some case lost data. 2: setup failed (no verdict).
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
rt=$(cd "${1:?runtime dir}" && pwd)
mkdir -p "${2:?work dir}"
work=$(cd "$2" && pwd)
cases_file=${3:-}
name=${VSOCK_TAIL_NAME:-vsock-tail}
msb=$rt/msb
export MSB_HOME=$work/home
sock=$work/route/tail.sock
results=$work/g2h.jsonl
mkdir -p "$work/route" "$work/guest"
[ ${#sock} -lt 100 ] || { echo "setup: socket path too long for AF_UNIX: $sock"; exit 2; }
cp "$here/vsock-tail.py" "$work/guest/"

default_cases() {
  cat <<'EOF'
h2g-close 1000 1048576
h2g-close 200 4096
h2g-close 200 65536
h2g-close 30 8388608
h2g-shutwr 200 1048576
h2g-close 20 4194304 delay=0.3 grate=4000000
g2h-close 1000 1048576
g2h-close 200 4096
g2h-close 5 8388608 rate=3200000
g2h-close 3 8388608 rate=400000
g2h-close 200 1048576 gbuf=0
g2h-close 3 8388608 rate=3200000 gbuf=0
EOF
}

host_pid=
# shellcheck disable=SC2329 # called by the EXIT trap
cleanup() {
  "$msb" stop "$name" >/dev/null 2>&1
  "$msb" rm "$name" >/dev/null 2>&1
  [ -n "$host_pid" ] && kill "$host_pid" 2>/dev/null
}
trap cleanup EXIT

echo "runtime: $("$msb" --version) ($msb)"
python3 "$work/guest/vsock-tail.py" host "$sock" "$results" >"$work/host.log" 2>&1 &
host_pid=$!
for _ in $(seq 50); do [ -S "$sock" ] && break; sleep 0.1; done
[ -S "$sock" ] || { echo "setup: host server did not start"; cat "$work/host.log"; exit 2; }

if ! "$msb" create --name "$name" --replace --cpus 2 --memory 1024 --no-net \
  --vsock "$sock:5000" --mount-dir "$work/guest:/opt/t:ro" python:3.12-alpine \
  >"$work/create.log" 2>&1; then
  echo "setup: msb create failed"; cat "$work/create.log"; exit 2
fi

# g2h_verdict <spec> <count> <size> <deadline s>: wait for the host's <count> records, then judge.
g2h_verdict() {
  python3 - "$results" "$2" "$3" "$4" <<'PY'
import json, sys, time
path, count, size, deadline = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), float(sys.argv[4])
end = time.monotonic() + deadline
recs = []
while time.monotonic() < end:
    with open(path) as f:
        recs = [json.loads(l) for l in f if l.strip()]
    if len(recs) >= count:
        break
    time.sleep(0.5)
bad = [r for r in recs if r["got"] != r["want"] or r["bad"] >= 0 or r["err"]]
for r in bad:
    print("  " + json.dumps(r))
short = sum(r["got"] < r["want"] for r in bad)
lost = sum(max(r["want"] - r["got"], 0) for r in recs) + (count - len(recs)) * size
secs = max((r["secs"] for r in recs), default=0)
print("RESULT %d %d %d %d %d %d %.1f" % (count, len(recs), short, sum(r["bad"] >= 0 for r in recs),
      sum(r["err"] is not None for r in recs), lost, secs))
PY
}

rows=()
fail=0
while read -r case count size opts; do
  [ -z "${case:-}" ] && continue
  case $case in \#*) continue ;; esac
  spec="$case $count $size${opts:+ $opts}"
  : >"$results"
  # shellcheck disable=SC2086 # opts is a list of k=v words
  out=$(timeout 900 "$msb" exec --no-tty --no-stdin "$name" -- \
    python3 /opt/t/vsock-tail.py guest "$case" "$count" "$size" $opts 2>&1 </dev/null)
  code=$?
  echo "$out" | grep -v '^SUMMARY ' | head -20
  if [ $code -ne 0 ] || ! summary=$(echo "$out" | grep '^SUMMARY ' | tail -1) || [ -z "$summary" ]; then
    echo "CASE $spec: ERROR (guest exit $code)"; echo "$out" | tail -5
    rows+=("| \`$spec\` | ERROR | guest exit $code | | |")
    fail=1
    continue
  fi
  if [ "$case" = g2h-close ]; then
    rate=$(sed -n 's/.*rate=\([0-9.]*\).*/\1/p' <<<"$opts")
    deadline=$(python3 -c "import sys; c,s,r=int(sys.argv[1]),int(sys.argv[2]),float(sys.argv[3] or 0); print(60 + (c*s/r if r else 0))" "$count" "$size" "${rate:-0}")
    v=$(g2h_verdict "$spec" "$count" "$size" "$deadline")
    echo "$v" | grep -v '^RESULT '
    read -r _ n logged short badc errs lost secs <<<"$(echo "$v" | grep '^RESULT ')"
    missing=$((n - logged))
  else
    read -r short badc errs lost < <(python3 -c 'import json,sys; s=json.loads(sys.argv[1][8:]); print(s["short"], s["bad"], s["errors"], s["lost"])' "$summary")
    missing=0
    secs=$(python3 -c 'import json,sys; print(json.loads(sys.argv[1][8:])["secs"])' "$summary")
  fi
  if [ "$short" -eq 0 ] && [ "$badc" -eq 0 ] && [ "$errs" -eq 0 ] && [ "$missing" -eq 0 ]; then
    verdict=PASS
  else
    verdict=LOSS; fail=1
  fi
  echo "CASE $spec: $verdict (short $short/$count, corrupt $badc, errors $errs, missing $missing, lost $lost B, ${secs}s)"
  rows+=("| \`$spec\` | $verdict | $short/$count short, $missing missing | $lost | $badc corrupt, $errs errors |")
done < <(if [ -n "$cases_file" ]; then cat "$cases_file"; else default_cases; fi)

table=$(printf '%s\n' "| Case | Verdict | Connections | Bytes lost | Other |" "|---|---|---|---|---|" "${rows[@]}")
echo "$table"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  { echo "### vsock tail, $("$msb" --version)"; echo; echo "$table"; echo; } >>"$GITHUB_STEP_SUMMARY"
fi
exit "$fail"
