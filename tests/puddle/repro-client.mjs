// SPDX-License-Identifier: Apache-2.0
// repro-client.mjs - host side. Opens TOTAL connections (PAR at a time) to a local `ssh -L`
// port, sends one byte and resets the socket (RST) a random 0..MAX_MS ms after connecting; if
// the guest closes first, that connection counts as "eof".
// The reset makes OpenSSH's pending channel write fail, so ssh sends CHANNEL_CLOSE without
// waiting for the guest's EOF, and `msb ssh serve` cancels the TCP bulk flow (BulkCancel).
// usage: node repro-client.mjs <local port> [total=3000] [par=32] [maxMs=20]
import net from "node:net";

const [port, total = 3000, par = 32, maxMs = 20] = process.argv.slice(2).map(Number);
const counts = { rst: 0, eof: 0, error: 0, stuck: 0, data: 0 };
const errorCodes = {};
let started = 0;

function one() {
  return new Promise((resolve) => {
    let done = false;
    const finish = (kind) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      counts[kind]++;
      resolve();
    };
    // A forward that neither resets nor closes within 5 s is counted, not waited for.
    let timer = setTimeout(() => {
      s.destroy();
      finish("stuck");
    }, 5000);
    const s = net.connect(port, "127.0.0.1", () => {
      s.write("g");
      // Reset at a random point, usually while ssh is opening the forward or the first guest
      // data is arriving. ssh's write to this socket then fails and it closes the channel.
      clearTimeout(timer);
      timer = setTimeout(() => {
        s.resetAndDestroy();
        finish("rst");
      }, Math.random() * maxMs);
    });
    let gotData = false;
    s.on("data", () => {
      if (!gotData) {
        gotData = true;
        counts.data++;
      }
    });
    s.on("end", () => {
      s.destroy();
      finish("eof");
    });
    s.on("error", (e) => {
      if (!done) errorCodes[e.code] = (errorCodes[e.code] ?? 0) + 1;
      finish("error");
    });
  });
}

async function worker() {
  while (started < total) {
    started++;
    await one();
  }
}

const t0 = Date.now();
await Promise.all(Array.from({ length: par }, worker));
console.log(
  `connections=${total} reset_by_client=${counts.rst} eof_from_guest=${counts.eof} ` +
    `stuck=${counts.stuck} errors=${counts.error} ${JSON.stringify(errorCodes)} got_data=${counts.data} ` +
    `seconds=${((Date.now() - t0) / 1000).toFixed(1)}`,
);
