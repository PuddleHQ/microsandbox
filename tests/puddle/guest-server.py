# SPDX-License-Identifier: Apache-2.0
# guest-server.py - runs inside the sandbox. For every connection: wait for one byte (or EOF),
# stream data for a random 0..MAX_MS milliseconds, then close. The guest-side EOF makes agentd queue a TCP
# BulkFinish at a moment that races the host client's own reset (repro-client.mjs).
import random, socket, sys, threading, time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18090
MAX_MS = float(sys.argv[2]) if len(sys.argv) > 2 else 20.0
CHUNK = b"x" * 16384
stats = {"accepted": 0, "got_byte": 0, "got_eof": 0, "streamed_and_closed": 0}


def count(key):
    # In memory only: a file write here would sit in the race window and hide the bug.
    stats[key] += 1


def dump_stats():
    while True:
        time.sleep(1)
        with open("/tmp/srv-stats.txt", "w") as f:
            f.write(" ".join(f"{k}={v}" for k, v in stats.items()) + "\n")


def handle(conn):
    try:
        # Stream even when the host already sent EOF (the client reset before ssh forwarded its
        # byte): the host then closes the channel while this side streams and closes.
        count("got_byte" if conn.recv(1) else "got_eof")
        end = time.monotonic() + random.uniform(0, MAX_MS) / 1000.0
        while time.monotonic() < end:
            conn.sendall(CHUNK)
        conn.close()
        count("streamed_and_closed")
    except OSError:
        conn.close()


threading.Thread(target=dump_stats, daemon=True).start()
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", PORT))
srv.listen(512)
while True:
    conn, _ = srv.accept()
    count("accepted")
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
