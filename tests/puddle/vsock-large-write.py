# SPDX-License-Identifier: Apache-2.0
# vsock-large-write.py - does a vsock route deliver a guest's bytes intact whatever size each
# write() call has? Linux 6.12.y guests build a write over about 32 KiB as a packet whose data
# spans several descriptors, one per page fragment. One script, two roles:
#
#   host  <socket path>              Unix-socket server on the host (the route's endpoint). Prints
#                                    "ready" once it listens. Per connection: reads the request line
#                                    "<total>\n", then exactly <total> bytes, and answers
#                                    "<bytes read> <sha256>\n" (and logs it to stderr).
#   guest <total> <chunk> [<chunk>..] client inside the sandbox. Per chunk size: connects to CID 2,
#                                    port 5000, sends <total> bytes in send() calls of <chunk> bytes,
#                                    and compares the host's answer with its own digest. Prints one
#                                    "WRITE {...}" line per chunk size. Exit 0 when every chunk size
#                                    arrived intact, 1 otherwise.
# guest options (k=v after the chunk sizes): port=<vsock port, 5000>, timeout=<s the guest waits for
# the answer, 30>, unix=<path> (a self-test of this script without a VM).
import hashlib, json, os, socket, sys, threading


def host(path):
    if os.path.exists(path):
        os.unlink(path)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(16)
    print("ready", flush=True)

    def serve(conn):
        with conn:
            f = conn.makefile("rb")
            line = f.readline()
            if not line.endswith(b"\n"):
                return
            want = int(line)
            h, got = hashlib.sha256(), 0
            while got < want:
                b = f.read1(min(1 << 20, want - got))
                if not b:
                    break
                h.update(b)
                got += len(b)
            print("host: want %d got %d sha256 %s" % (want, got, h.hexdigest()), file=sys.stderr, flush=True)
            conn.sendall(b"%d %s\n" % (got, h.hexdigest().encode()))

    while True:
        conn, _ = srv.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


def one(total, chunk, opts):
    if "unix" in opts:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        addr = opts["unix"]
    else:
        s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        addr = (2, int(opts.get("port", 5000)))
    s.settimeout(float(opts.get("timeout", 30)))
    # Bytes that differ per offset, so a lost, doubled or reordered packet changes the digest.
    data = hashlib.shake_256(b"vsock-large-write %d" % chunk).digest(total)
    res = {"chunk": chunk, "total": total, "want": hashlib.sha256(data).hexdigest()}
    try:
        s.connect(addr)
        s.sendall(b"%d\n" % total)
        view = memoryview(data)
        for off in range(0, total, chunk):
            s.sendall(view[off:off + chunk])
        answer = s.makefile("rb").readline().split()
        res["got_bytes"], res["got"] = int(answer[0]), answer[1].decode()
    except (OSError, IndexError, ValueError) as err:
        res["error"] = "%s: %s" % (type(err).__name__, err)
    finally:
        s.close()
    res["ok"] = res.get("got") == res["want"]
    return res


def guest(argv):
    sizes = [int(a) for a in argv if "=" not in a]
    opts = dict(a.split("=", 1) for a in argv if "=" in a)
    total, chunks = sizes[0], sizes[1:]
    ok = True
    for chunk in chunks:
        res = one(total, chunk, opts)
        print("WRITE " + json.dumps(res), flush=True)
        ok = ok and res["ok"]
    return 0 if ok else 1


def main(argv):
    if len(argv) == 2 and argv[0] == "host":
        host(argv[1])
    elif len(argv) >= 3 and argv[0] == "guest":
        sys.exit(guest(argv[1:]))
    else:
        sys.exit("usage: vsock-large-write.py host <sock> | guest <total> <chunk>.. [k=v..]")


if __name__ == "__main__":
    main(sys.argv[1:])
