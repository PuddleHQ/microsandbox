// SPDX-License-Identifier: Apache-2.0
// wedge-client.mjs - host side of the russh peer-close wedge repro (puddle T-071/T-082, mode
// "rstdata" of poc/forward-churn/churn-client.mjs). Opens TOTAL connections (PAR at a time) to a
// local `ssh -L` port; each sends one byte and resets the socket 0..MAX_MS ms after the FIRST data
// arrives, while the guest is still streaming. OpenSSH then closes the channel with data queued
// behind the window; stock russh 0.63.3 stops sending on every channel of the session.
// A connection with a gap > 5 s between events is "stalled"; one with no end after HARD s is
// "hung". Prints one summary line, then up to 8 stall samples.
// usage: node wedge-client.mjs <port> [total=300] [par=32] [maxMs=20] [hardSec=30]
import net from "node:net";

const [portS, totalS = "300", parS = "32", maxMsS = "20", hardS = "30"] = process.argv.slice(2);
const port = Number(portS), total = Number(totalS), par = Number(parS), maxMs = Number(maxMsS);
const hardMs = Number(hardS) * 1000;
const STALL_MS = 5000;
const counts = { rst: 0, eof: 0, error: 0, hung: 0, stalled: 0, data: 0 };
const errorCodes = {};
const stalls = [];
let started = 0, maxGap = 0;
const t0 = Date.now();

function one(idx) {
  return new Promise((resolve) => {
    let last = Date.now(), gap = 0, gapAt = 0, done = false, gotData = false, rstTimer = null;
    const mark = () => {
      const now = Date.now();
      if (now - last > gap) { gap = now - last; gapAt = last - t0; }
      last = now;
    };
    const finish = (kind) => {
      if (done) return;
      mark();
      done = true;
      clearTimeout(hard);
      clearTimeout(rstTimer);
      counts[kind]++;
      if (gap > maxGap) maxGap = gap;
      if (gap > STALL_MS) {
        counts.stalled++;
        if (stalls.length < 8) stalls.push({ idx, kind, gapMs: gap, gapStartMs: gapAt, gotData });
      }
      resolve();
    };
    const hard = setTimeout(() => { s.destroy(); finish("hung"); }, hardMs);
    const s = net.connect(port, "127.0.0.1", () => { mark(); s.write("g"); });
    s.on("data", () => {
      mark();
      if (!gotData) {
        gotData = true;
        counts.data++;
        rstTimer = setTimeout(() => { s.resetAndDestroy(); finish("rst"); }, Math.random() * maxMs);
      }
    });
    s.on("end", () => { s.destroy(); finish("eof"); });
    s.on("error", (e) => {
      if (!done) errorCodes[e.code] = (errorCodes[e.code] ?? 0) + 1;
      finish("error");
    });
  });
}

async function worker() {
  while (started < total) await one(started++);
}

await Promise.all(Array.from({ length: par }, worker));
console.log(
  `n=${total} par=${par} rst=${counts.rst} eof=${counts.eof} err=${counts.error} ${JSON.stringify(errorCodes)} ` +
    `hung=${counts.hung} STALLED=${counts.stalled} data=${counts.data} maxGapMs=${maxGap} ` +
    `s=${((Date.now() - t0) / 1000).toFixed(1)}`,
);
for (const st of stalls) console.log("  stall " + JSON.stringify(st));
