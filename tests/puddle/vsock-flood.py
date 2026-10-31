# SPDX-License-Identifier: Apache-2.0
# vsock-flood.py - what does a guest that opens vsock connections in a loop and writes without
# reading cost the host? One script, two roles:
#
#   host  <socket path>                  Unix-socket server (the route's endpoint). It accepts every
#                                        connection, reads the request line and then never reads
#                                        again, like a peer that is stuck.
#   guest <streams> [k=v..]              client inside the sandbox: opens <streams> connections to
#                                        CID 2, PORT, one after the other. On each it sends a
#                                        request line and then writes until the kernel refuses more
#                                        (no credit). It holds every connection open for hold=<s>.
#
# guest options: port=<vsock port, 5000>, chunk=<B per send, 65536>, hold=<s, 5>, gbuf=<guest vsock
# buffer, 16 MiB as the puddle agent sets it>, unix=<path> (a self-test without a VM).
# The guest prints "FLOOD {...}": connections opened, refused (connect failed or reset), bytes the
# guest got onto the route, and the first error text.
import fcntl, json, os, socket, struct, sys, termios, threading, time


def host(path):
    if os.path.exists(path):
        os.unlink(path)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(4096)
    print("host: listening on", path, flush=True)

    conns = []

    def watch():
        # Bytes the host's kernel holds unread in the accepted connections (what the guest cost it).
        while True:
            total = 0
            for c in list(conns):
                try:
                    total += struct.unpack("i", fcntl.ioctl(c, termios.FIONREAD, b"\0\0\0\0"))[0]
                except OSError:
                    pass
            print("queued_bytes", total, "conns", len(conns), flush=True)
            time.sleep(0.5)

    threading.Thread(target=watch, daemon=True).start()

    def hold(conn):
        conns.append(conn)
        try:
            line = b""
            while not line.endswith(b"\n"):
                b = conn.recv(1)
                if not b:
                    return
                line += b
            while True:
                time.sleep(3600)
        except OSError:
            pass

    while True:
        conn, _ = srv.accept()
        threading.Thread(target=hold, args=(conn,), daemon=True).start()


def sink(path):
    """Route endpoint that reads everything (a healthy peer), for the throughput role."""
    if os.path.exists(path):
        os.unlink(path)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(64)
    print("host: sinking on", path, flush=True)

    def drain(conn):
        try:
            while conn.recv(1 << 20):
                pass
        except OSError:
            pass

    while True:
        conn, _ = srv.accept()
        threading.Thread(target=drain, args=(conn,), daemon=True).start()


def upload(mib, opts):
    """One connection, <mib> MiB written as fast as the route takes it; prints UPLOAD with MiB/s."""
    port = int(opts.get("port", 5000))
    chunk = int(opts.get("chunk", 1 << 20))
    gbuf = int(opts.get("gbuf", 16 << 20))
    s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    s.setsockopt(socket.AF_VSOCK, socket.SO_VM_SOCKETS_BUFFER_MAX_SIZE, gbuf)
    s.setsockopt(socket.AF_VSOCK, socket.SO_VM_SOCKETS_BUFFER_SIZE, gbuf)
    s.connect((2, port))
    s.sendall(b"upload\n")
    payload = b"\0" * chunk
    start = time.time()
    for _ in range(mib * (1 << 20) // chunk):
        s.sendall(payload)
    # sendall returning means the route took every byte but the last window; no wait for a close.
    took = time.time() - start
    print("UPLOAD " + json.dumps({"mib": mib, "seconds": round(took, 2), "mib_per_s": round(mib / took, 1)}),
          flush=True)


def fill(s, payload):
    """Send until the kernel takes no more; returns the bytes sent and an error text or None."""
    sent = 0
    try:
        while True:
            sent += s.send(payload)
    except BlockingIOError:
        return sent, None
    except OSError as err:
        return sent, "%s: %s" % (type(err).__name__, err)


def meminfo_mib():
    """MemFree and SUnreclaim of this machine in MiB (the guest's own cost of the flood)."""
    out = {}
    try:
        with open("/proc/meminfo") as f:
            for line in f:
                key, _, rest = line.partition(":")
                if key in ("MemFree", "SUnreclaim"):
                    out[key] = int(rest.split()[0]) // 1024
    except OSError:
        pass
    return out


def guest(streams, opts):
    port = int(opts.get("port", 5000))
    chunk = int(opts.get("chunk", 65536))
    hold = float(opts.get("hold", 5))
    gbuf = int(opts.get("gbuf", 16 << 20))
    payload = b"\0" * chunk
    held, refused, sent, first_err = [], 0, 0, None
    mem0 = meminfo_mib()
    for i in range(streams):
        try:
            if "unix" in opts:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.connect(opts["unix"])
            else:
                s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
                s.setsockopt(socket.AF_VSOCK, socket.SO_VM_SOCKETS_BUFFER_MAX_SIZE, gbuf)
                s.setsockopt(socket.AF_VSOCK, socket.SO_VM_SOCKETS_BUFFER_SIZE, gbuf)
                s.settimeout(10)
                s.connect((2, port))
            s.sendall(b"flood %d\n" % i)
        except OSError as err:
            refused += 1
            first_err = first_err or "%s: %s" % (type(err).__name__, err)
            continue
        s.setblocking(False)
        held.append(s)
        n, err = fill(s, payload)
        sent += n
        first_err = first_err or err
    # A second pass: credit that came back while later connections were opening.
    time.sleep(1)
    for s in held:
        n, _ = fill(s, payload)
        sent += n
    print("FLOOD " + json.dumps({"streams": streams, "opened": len(held), "refused": refused,
                                 "sent_bytes": sent, "first_error": first_err,
                                 "guest_mem_before_mib": mem0, "guest_mem_after_mib": meminfo_mib()}),
          flush=True)
    time.sleep(hold)


def main(argv):
    if len(argv) == 2 and argv[0] == "host":
        host(argv[1])
    elif len(argv) == 2 and argv[0] == "sink":
        sink(argv[1])
    elif len(argv) >= 2 and argv[0] == "upload":
        upload(int(argv[1]), dict(a.split("=", 1) for a in argv[2:]))
    elif len(argv) >= 2 and argv[0] == "guest":
        guest(int(argv[1]), dict(a.split("=", 1) for a in argv[2:]))
    else:
        sys.exit("usage: vsock-flood.py host <sock> | guest <streams> [k=v..]")


if __name__ == "__main__":
    main(sys.argv[1:])
