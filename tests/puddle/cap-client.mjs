// SPDX-License-Identifier: Apache-2.0
// cap-client.mjs - host side of the forward-cap repro. Opens N connections at once to a local
// `ssh -L` port whose guest server (hold-server.py) greets each one and holds it open, so N
// forwards are active on one ssh session. Stock msb refuses every forward past the 64th: the
// connection is reset, or (through VS Code's forwarder) hangs.
// With RELEASE > 0, the first RELEASE greeted connections are closed RELEASE_AFTER_MS after the
// start, which frees their slots for connections that msb is still retrying.
// A connection that is not greeted within HARD_MS counts as "timeout".
// usage: node cap-client.mjs <port> <n> [release=0] [releaseAfterMs=4000] [hardMs=40000]
// Prints one summary line:
//   n=<n> greeted=<g> reset=<r> closed=<c> timeout=<t> maxGreetMs=<ms> lateGreeted=<k> releasedAtMs=<ms>
// lateGreeted: connections greeted after the release started, i.e. the ones msb queued.
import net from "node:net";

const [portS, nS, releaseS = "0", releaseAfterS = "4000", hardS = "40000"] = process.argv.slice(2);
const port = Number(portS), n = Number(nS), release = Number(releaseS);
const releaseAfterMs = Number(releaseAfterS), hardMs = Number(hardS);
const r = { greeted: 0, reset: 0, closed: 0, timeout: 0 };
const greeted = [];
const t0 = Date.now();
let maxGreetMs = 0, lateGreeted = 0, releasedAt = -1;

const all = Array.from({ length: n }, () => new Promise((resolve) => {
  const s = net.connect(port, "127.0.0.1");
  let done = false;
  const fin = (kind) => {
    if (done) return;
    done = true;
    clearTimeout(timer);
    r[kind]++;
    resolve();
  };
  const timer = setTimeout(() => { s.destroy(); fin("timeout"); }, hardMs);
  s.once("data", () => {
    const ms = Date.now() - t0;
    maxGreetMs = Math.max(maxGreetMs, ms);
    if (releasedAt >= 0) lateGreeted++;
    greeted.push(s);
    fin("greeted");
  });
  s.on("error", () => fin("reset"));
  s.on("close", () => fin("closed"));
}));

if (release > 0) {
  setTimeout(() => {
    releasedAt = Date.now() - t0;
    for (const s of greeted.slice(0, release)) s.destroy();
  }, releaseAfterMs);
}

await Promise.all(all);
console.log(
  `n=${n} greeted=${r.greeted} reset=${r.reset} closed=${r.closed} timeout=${r.timeout} ` +
    `maxGreetMs=${maxGreetMs} lateGreeted=${lateGreeted} releasedAtMs=${releasedAt}`,
);
for (const s of greeted) s.destroy();
process.exit(0);
