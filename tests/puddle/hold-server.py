# SPDX-License-Identifier: Apache-2.0
# hold-server.py - runs inside the sandbox. Greets every connection with "hi\n", then keeps it open
# until the client closes it. Used by the forward-cap repro (cap-client.mjs): each held connection
# holds one active forward on the ssh session.
import socket, sys, threading

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 18095


def handle(conn):
    try:
        conn.sendall(b"hi\n")
        while conn.recv(4096):
            pass
    except OSError:
        pass
    finally:
        conn.close()


srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", PORT))
srv.listen(1024)
while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
