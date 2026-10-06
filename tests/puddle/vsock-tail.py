# SPDX-License-Identifier: Apache-2.0
# vsock-tail.py - does a vsock route (guest AF_VSOCK -> host Unix socket) deliver every byte
# before EOF when one side closes right after its last write? One script, two roles:
#
#   host  <socket path> <results file>   Unix-socket server on the host (the route's endpoint).
#   guest <case> <count> <size> [k=v..]  client inside the sandbox; connects to CID 2, PORT.
#
# Every connection starts with one request line from the guest, "<case> <size> <id> <rate>\n".
# Payload bytes are PATTERN[(offset) % 251], so a short stream, a gap or a corruption all show.
#
# Cases (which side closes, who reads):
#   h2g-close   host writes <size>, then close() at once; the guest reads to EOF (microsandbox#1753)
#   h2g-shutwr  host writes <size>, shutdown(SHUT_WR), waits for the guest to close
#   g2h-close   guest writes <size>, then close() at once; the host reads to EOF at <rate> B/s
#               (0 = as fast as it can) and logs the result
# guest options: rate=<B/s> (host read rate, g2h), delay=<s> (guest waits before its first read,
# h2g), grate=<B/s> (guest read rate, h2g), chunk=<B> (guest send size, g2h), port=<vsock port>,
# gbuf=<B> (guest vsock buffer for g2h, default 16 MiB; 0 keeps the kernel's 256 KiB),
# unix=<path> (connect to a Unix socket instead of vsock: a self-test of this script without a VM).
# The guest prints one JSON line per short or bad connection and a final "SUMMARY {...}" line;
# the host appends one JSON line per g2h connection to <results file>.
import json, os, socket, sys, threading, time

PERIOD = 251
PATTERN = bytes(range(PERIOD)) * ((1 << 20) // PERIOD + 2)
READ = 65536


def expected(offset, n):
    start = offset % PERIOD
    return PATTERN[start:start + n]


def check(buf, offset):
    """Index of the first byte in buf that breaks the pattern, or -1."""
    want = expected(offset, len(buf))
    if buf == want:
        return -1
    return next(i for i, (a, b) in enumerate(zip(buf, want)) if a != b)


def send_pattern(sock, size, chunk):
    sent = 0
    while sent < size:
        n = min(chunk, size - sent)
        sock.sendall(expected(sent, n))
        sent += n


def read_all(sock, rate=0.0, delay=0.0):
    """Read to EOF; returns (bytes, first bad offset or -1, error text or None)."""
    if delay:
        time.sleep(delay)
    got, bad, start = 0, -1, time.monotonic()
    try:
        while True:
            if rate:
                ahead = got / rate - (time.monotonic() - start)
                if ahead > 0:
                    time.sleep(ahead)
            buf = sock.recv(READ)
            if not buf:
                return got, bad, None
            if bad < 0:
                i = check(buf, got)
                if i >= 0:
                    bad = got + i
            got += len(buf)
    except OSError as err:
        return got, bad, "%s: %s" % (type(err).__name__, err)


def read_line(sock):
    line = b""
    while not line.endswith(b"\n"):
        b = sock.recv(1)
        if not b:
            raise EOFError("request line cut short: %r" % line)
        line += b
    return line.decode().split()


# ---- host ----------------------------------------------------------------------------------------

def host_conn(conn, results, lock):
    try:
        case, size, cid, rate = read_line(conn)
        size, rate = int(size), float(rate)
        if case == "h2g-close":
            send_pattern(conn, size, READ)
            conn.close()
            return
        if case == "h2g-shutwr":
            send_pattern(conn, size, READ)
            conn.shutdown(socket.SHUT_WR)
            while conn.recv(READ):
                pass
            conn.close()
            return
        if case == "g2h-close":
            t0 = time.monotonic()
            got, bad, err = read_all(conn, rate)
            conn.close()
            rec = {"case": case, "id": int(cid), "want": size, "got": got, "bad": bad, "err": err,
                   "secs": round(time.monotonic() - t0, 3)}
            with lock, open(results, "a") as f:
                f.write(json.dumps(rec) + "\n")
            return
        raise ValueError("unknown case " + case)
    except Exception as err:  # one broken connection must not stop the server
        sys.stderr.write("host: connection failed: %r\n" % err)
        try:
            conn.close()
        except OSError:
            pass


def host(path, results):
    if os.path.exists(path):
        os.unlink(path)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(128)
    lock = threading.Lock()
    print("host: listening on", path, flush=True)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=host_conn, args=(conn, results, lock), daemon=True).start()


# ---- guest ---------------------------------------------------------------------------------------

def guest(case, count, size, opts):
    port = int(opts.get("port", 5000))
    rate = float(opts.get("rate", 0))
    delay = float(opts.get("delay", 0))
    grate = float(opts.get("grate", 0))
    chunk = int(opts.get("chunk", 2048))
    gbuf = int(opts.get("gbuf", 16 << 20))
    short = bad = errors = lost = 0
    t0 = time.monotonic()
    for i in range(count):
        if "unix" in opts:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(opts["unix"])
        else:
            s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        if case == "g2h-close" and gbuf and "unix" not in opts:
            # As in the earlier Windows runs: a large guest buffer keeps the 256 KiB credit stall
            # of older runtimes out of the way, so only the close path is measured.
            s.setsockopt(socket.AF_VSOCK, socket.SO_VM_SOCKETS_BUFFER_MAX_SIZE, gbuf)
            s.setsockopt(socket.AF_VSOCK, socket.SO_VM_SOCKETS_BUFFER_SIZE, gbuf)
        if "unix" not in opts:
            s.connect((2, port))
        s.sendall(("%s %d %d %s\n" % (case, size, i, rate)).encode())
        if case == "g2h-close":
            send_pattern(s, size, chunk)
            s.close()
            continue
        got, b, err = read_all(s, grate, delay)
        s.close()
        if got != size or b >= 0 or err:
            short += got < size
            bad += b >= 0
            errors += err is not None
            lost += max(size - got, 0)
            print(json.dumps({"case": case, "id": i, "want": size, "got": got, "bad": b, "err": err}),
                  flush=True)
    summary = {"case": case, "count": count, "size": size, "secs": round(time.monotonic() - t0, 1)}
    if case != "g2h-close":
        summary.update(short=short, bad=bad, errors=errors, lost=lost)
    print("SUMMARY " + json.dumps(summary), flush=True)


def main(argv):
    if len(argv) == 3 and argv[0] == "host":
        host(argv[1], argv[2])
    elif len(argv) >= 4 and argv[0] == "guest":
        opts = dict(a.split("=", 1) for a in argv[4:])
        guest(argv[1], int(argv[2]), int(argv[3]), opts)
    else:
        sys.exit(__doc__ or "usage: vsock-tail.py host <sock> <results> | guest <case> <count> <size> [k=v..]")


if __name__ == "__main__":
    main(sys.argv[1:])
