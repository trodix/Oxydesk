#!/usr/bin/env python3
"""TCP proxy 127.0.0.1:3391 -> 127.0.0.1:3390; SIGUSR1 resets every open connection (a network loss seen by the client)."""
import signal, socket, struct, threading, sys
conns = []
lock = threading.Lock()
def pump(a, b):
    try:
        while (d := a.recv(65536)):
            b.sendall(d)
    except OSError:
        pass
    for s in (a, b):
        try: s.shutdown(socket.SHUT_RDWR)
        except OSError: pass
def cut(*_):
    with lock:
        for s in conns:
            try:
                s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0)); s.close()
            except OSError: pass
        conns.clear()
    print("cut", flush=True)
signal.signal(signal.SIGUSR1, cut)
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 3391)); srv.listen(16)
print("ready", flush=True)
while True:
    try: c, _ = srv.accept()
    except InterruptedError: continue
    u = socket.create_connection(("127.0.0.1", 3390))
    with lock: conns.extend([c, u])
    threading.Thread(target=pump, args=(c, u), daemon=True).start()
    threading.Thread(target=pump, args=(u, c), daemon=True).start()
