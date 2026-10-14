#!/usr/bin/env python3
"""TCP proxy that shapes a link: bandwidth cap and one-way delay, like a slow network between a client and the server.

usage: throttle-proxy.py LISTEN_ADDR:PORT TARGET_ADDR:PORT RATE_KBIT DELAY_MS [UP_RATE_KBIT]

RATE_KBIT caps the server -> client direction (the video), DELAY_MS is added in each direction. A byte only leaves the
proxy once the link has had the time to carry what is before it; the proxy stops reading its source when 64 KiB wait,
so that the sender's own socket fills up, as it would behind a real bottleneck. 0 = no cap.
"""
import asyncio
import sys
import time

BUFFER = 64 * 1024
CHUNK = 8 * 1024


async def pipe(reader, writer, rate_kbit, delay_s):
    queue = asyncio.Queue()
    waiting = 0
    room = asyncio.Event()
    room.set()
    free_at = 0.0

    async def deliver():
        nonlocal waiting
        try:
            while True:
                at, data = await queue.get()
                if data is None:
                    break
                lag = at - time.monotonic()
                if lag > 0:
                    await asyncio.sleep(lag)
                writer.write(data)
                await writer.drain()
                waiting -= len(data)
                if waiting < BUFFER:
                    room.set()
        except (ConnectionError, asyncio.CancelledError):
            pass
        finally:
            writer.close()

    task = asyncio.create_task(deliver())
    try:
        while True:
            await room.wait()
            data = await reader.read(CHUNK)
            if not data:
                break
            now = time.monotonic()
            if rate_kbit:
                start = max(now, free_at)
                free_at = start + len(data) * 8 / (rate_kbit * 1000)
            else:
                free_at = now
            waiting += len(data)
            if waiting >= BUFFER:
                room.clear()
            queue.put_nowait((free_at + delay_s, data))
    except (ConnectionError, asyncio.CancelledError):
        pass
    finally:
        queue.put_nowait((0, None))
        await task


async def main():
    listen, target = sys.argv[1], sys.argv[2]
    down, delay = int(sys.argv[3]), int(sys.argv[4]) / 1000
    up = int(sys.argv[5]) if len(sys.argv) > 5 else 0
    lhost, lport = listen.rsplit(":", 1)
    thost, tport = target.rsplit(":", 1)

    async def handle(client_r, client_w):
        try:
            server_r, server_w = await asyncio.open_connection(thost, int(tport))
        except OSError:
            client_w.close()
            return
        await asyncio.gather(
            pipe(client_r, server_w, up, delay),
            pipe(server_r, client_w, down, delay),
        )

    server = await asyncio.start_server(handle, lhost, int(lport))
    async with server:
        await server.serve_forever()


asyncio.run(main())
